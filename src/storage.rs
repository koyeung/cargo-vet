use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, BufReader, Read, Seek, Write},
    mem,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use cargo_metadata::semver;
use flate2::read::GzDecoder;
use futures_util::future::try_join_all;
use miette::SourceOffset;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use similar::{udiff::unified_diff, Algorithm};
use tar::Archive;
use tracing::{error, info, log::warn, trace};

use crate::{
    criteria::CriteriaMapper,
    errors::{
        BadFormatError, BadWildcardEndDateError, CacheAcquireError, CacheCommitError, CertifyError,
        CommandError, CriteriaChangeError, CriteriaChangeErrors, DiffError, DownloadError,
        FetchAndDiffError, FetchAuditError, FetchError, FlockError, InvalidCriteriaError,
        JsonParseError, LoadJsonError, LoadTomlError, SourceFile, StoreAcquireError,
        StoreCommitError, StoreCreateError, StoreJsonError, StoreTomlError, StoreValidateError,
        StoreValidateErrors, TomlParseError, UnpackCheckoutError, UnpackError,
    },
    flock::{FileLock, Filesystem},
    format::{
        self, AuditEntry, AuditKind, AuditedDependencies, AuditsFile, CommandHistory, ConfigFile,
        CratesAPICrate, CratesPublisher, CriteriaEntry, CriteriaName, Delta, DiffCache, DiffStat,
        FastMap, FastSet, FetchCommand, ForeignAuditsFile, ImportName, ImportsFile, MetaConfig,
        PackageName, PackageStr, PublisherCache, PublisherCacheEntry, PublisherCacheUser,
        PublisherCacheVersion, SortedMap, VetVersion, WildcardAudits, WildcardEntry,
        SAFE_TO_DEPLOY, SAFE_TO_RUN,
    },
    network::Network,
    out::{indeterminate_spinner, progress_bar, IncProgressOnDrop},
    serialization::{parse_from_value, spanned::Spanned, to_formatted_toml},
    Config, PackageExt, PartialConfig, CARGO_ENV,
};

/// The type to use for accessing information from crates.io.
#[cfg(not(test))]
type CratesIndex = crates_index::Index;

/// When running tests, a mock index is used instead of the real one.
#[cfg(test)]
type CratesIndex = crate::tests::MockIndex;

// tmp cache for various shenanigans
const CACHE_DIFF_CACHE: &str = "diff-cache.toml";
const CACHE_COMMAND_HISTORY: &str = "command-history.json";
const CACHE_PUBLISHER_CACHE: &str = "publisher-cache.json";
const CACHE_EMPTY_PACKAGE: &str = "empty";
const CACHE_REGISTRY_SRC: &str = "src";
const CACHE_REGISTRY_CACHE: &str = "cache";
const CACHE_VET_LOCK: &str = ".vet-lock";

// Files which are allowed to appear in the root of the cache directory, and
// will not be GC'd
const CACHE_ALLOWED_FILES: &[&str] = &[
    CACHE_DIFF_CACHE,
    CACHE_COMMAND_HISTORY,
    CACHE_PUBLISHER_CACHE,
    CACHE_EMPTY_PACKAGE,
    CACHE_REGISTRY_SRC,
    CACHE_REGISTRY_CACHE,
    CACHE_VET_LOCK,
];

// Various cargo values
const CARGO_REGISTRY_SRC: &str = "src";
const CARGO_REGISTRY_CACHE: &str = "cache";
const CARGO_TOML_FILE: &str = "Cargo.toml";
const CARGO_OK_FILE: &str = ".cargo-ok";
const CARGO_OK_BODY: &str = "ok";

pub const DEFAULT_STORE: &str = "supply-chain";

const AUDITS_TOML: &str = "audits.toml";
const CONFIG_TOML: &str = "config.toml";
const IMPORTS_LOCK: &str = "imports.lock";

// Files which are skipped when counting changes for diffs.
const DIFF_SKIP_PATHS: &[&str] = &["Cargo.lock", ".cargo_vcs_info.json", ".cargo-ok"];

// FIXME: This is a completely arbitrary number, and may be too high or too low.
const MAX_CONCURRENT_DIFFS: usize = 40;

// Re-check if a relevant version was not published every day.
const NONINDEX_VERSION_PUBLISHER_REFRESH_DAYS: i64 = 1;

struct StoreLock {
    config: FileLock,
}

impl StoreLock {
    fn new(store: &Filesystem) -> Result<Self, FlockError> {
        Ok(StoreLock {
            config: store.open_rw(CONFIG_TOML, "vet store")?,
        })
    }
    fn read_config(&self) -> io::Result<impl Read + '_> {
        let mut file = self.config.file();
        file.rewind()?;
        Ok(file)
    }
    fn write_config(&self) -> io::Result<impl Write + '_> {
        let mut file = self.config.file();
        file.rewind()?;
        file.set_len(0)?;
        Ok(file)
    }
    fn read_audits(&self) -> io::Result<impl Read> {
        File::open(self.config.parent().join(AUDITS_TOML))
    }
    fn write_audits(&self) -> io::Result<impl Write> {
        File::create(self.config.parent().join(AUDITS_TOML))
    }
    fn read_imports(&self) -> io::Result<impl Read> {
        File::open(self.config.parent().join(IMPORTS_LOCK))
    }
    fn write_imports(&self) -> io::Result<impl Write> {
        File::create(self.config.parent().join(IMPORTS_LOCK))
    }
}

/// The store (typically `supply-chain/`)
///
/// All access to this directory should be managed by this type to avoid races.
/// By default, modifications to this type will not be written back to the store
/// because we don't generally want to write back any results unless everything
/// goes perfectly.
///
/// To write back this value, use [`Store::commit`][].
pub struct Store {
    // Exclusive file lock held for the config file
    lock: Option<StoreLock>,

    // Contents of the store, eagerly loaded and already validated.
    pub config: ConfigFile,
    pub imports: ImportsFile,
    pub audits: AuditsFile,

    // The complete live set of imports fetched from the network. Will be
    // initialized to `None` if `--locked` was passed.
    pub live_imports: Option<ImportsFile>,

    pub config_src: SourceFile,
    pub imports_src: SourceFile,
    pub audits_src: SourceFile,
}

impl Store {
    /// Create a new store (files will be completely empty, must be committed for files to be created)
    pub fn create(cfg: &Config) -> Result<Self, StoreCreateError> {
        let root = cfg.metacfg.store_path();
        root.create_dir().map_err(StoreCreateError::CouldntCreate)?;

        let lock = StoreLock::new(&root)?;

        Ok(Self {
            lock: Some(lock),
            config: ConfigFile {
                default_criteria: String::new(),
                imports: SortedMap::new(),
                policy: Default::default(),
                exemptions: SortedMap::new(),
            },
            imports: ImportsFile {
                publisher: SortedMap::new(),
                audits: SortedMap::new(),
            },
            audits: AuditsFile {
                criteria: SortedMap::new(),
                wildcard_audits: SortedMap::new(),
                audits: SortedMap::new(),
            },
            live_imports: None,
            config_src: SourceFile::new_empty(CONFIG_TOML),
            audits_src: SourceFile::new_empty(AUDITS_TOML),
            imports_src: SourceFile::new_empty(IMPORTS_LOCK),
        })
    }

    pub fn is_init(metacfg: &MetaConfig) -> bool {
        // Probably want to do more here later...
        metacfg.store_path().as_path_unlocked().exists()
    }

    pub fn acquire_offline(cfg: &Config) -> Result<Self, StoreAcquireError> {
        Self::acquire(cfg, None, false)
    }

    /// Acquire an existing store
    ///
    /// If `network` is passed and `!cfg.cli.locked`, this will fetch remote
    /// imports to use for comparison purposes.
    pub fn acquire(
        cfg: &Config,
        network: Option<&Network>,
        allow_criteria_changes: bool,
    ) -> Result<Self, StoreAcquireError> {
        let root = cfg.metacfg.store_path();

        // Before we do anything else, acquire an exclusive lock on the
        // config.toml file in the store.
        // XXX: Consider acquiring a non-exclusive lock in cases where an
        // exclusive one isn't needed.
        let lock = StoreLock::new(&root)?;

        let (config_src, config): (_, ConfigFile) = load_toml(CONFIG_TOML, lock.read_config()?)?;
        let (audits_src, audits): (_, AuditsFile) = load_toml(AUDITS_TOML, lock.read_audits()?)?;
        let (imports_src, imports): (_, ImportsFile) =
            load_toml(IMPORTS_LOCK, lock.read_imports()?)?;

        // If this command isn't locked, and the network is available, fetch the
        // live state of imported audits.
        let live_imports = if let (false, Some(network)) = (cfg.cli.locked, network) {
            let fetched_audits = tokio::runtime::Handle::current()
                .block_on(fetch_imported_audits(network, &config))?;
            let mut live_imports = process_imported_audits(
                fetched_audits,
                &audits,
                &config,
                &imports,
                allow_criteria_changes,
            )?;
            // XXX: Allow callers to hold on to this cache so we don't need to
            // re-acquire it after `Store::acquire`.
            let cache = Cache::acquire(cfg).map_err(Box::new)?;
            import_publisher_versions(
                &cfg.metadata,
                network,
                &cache,
                &wildcard_audits_packages(&audits, &live_imports),
                &config,
                &audits,
                &imports,
                &mut live_imports,
            )
            .map_err(Box::new)?;
            Some(live_imports)
        } else {
            None
        };

        let store = Self {
            lock: Some(lock),
            config,
            audits,
            imports,
            live_imports,
            config_src,
            audits_src,
            imports_src,
        };

        // We need to know today's date to validate that end dates are valid.
        let today = <chrono::DateTime<chrono::Utc>>::from(SystemTime::now()).date_naive();

        // Check that the store isn't corrupt
        store.validate(today, cfg.cli.locked)?;

        Ok(store)
    }

    /// Create a mock store
    #[cfg(test)]
    pub fn mock(config: ConfigFile, audits: AuditsFile, imports: ImportsFile) -> Self {
        Self {
            lock: None,
            config,
            imports,
            audits,
            live_imports: None,
            config_src: SourceFile::new_empty(CONFIG_TOML),
            audits_src: SourceFile::new_empty(AUDITS_TOML),
            imports_src: SourceFile::new_empty(IMPORTS_LOCK),
        }
    }

    /// Create a mock store, also mocking out the unlocked import fetching
    /// process by providing a mocked `Network` instance.
    ///
    /// NOTE: When validating the store, `mock_online` will use a "today" date
    /// of 2023-01-01.
    #[cfg(test)]
    pub fn mock_online(
        cfg: &Config,
        config: ConfigFile,
        audits: AuditsFile,
        imports: ImportsFile,
        network: &Network,
        allow_criteria_changes: bool,
    ) -> Result<Self, StoreAcquireError> {
        let fetched_audits =
            tokio::runtime::Handle::current().block_on(fetch_imported_audits(network, &config))?;
        let mut live_imports = process_imported_audits(
            fetched_audits,
            &audits,
            &config,
            &imports,
            allow_criteria_changes,
        )?;
        let cache = Cache::acquire(cfg).map_err(Box::new)?;
        import_publisher_versions(
            &cfg.metadata,
            network,
            &cache,
            &wildcard_audits_packages(&audits, &live_imports),
            &config,
            &audits,
            &imports,
            &mut live_imports,
        )
        .map_err(Box::new)?;

        let store = Self {
            lock: None,
            config,
            imports,
            audits,
            live_imports: Some(live_imports),
            config_src: SourceFile::new_empty(CONFIG_TOML),
            audits_src: SourceFile::new_empty(AUDITS_TOML),
            imports_src: SourceFile::new_empty(IMPORTS_LOCK),
        };

        let today = chrono::NaiveDate::from_ymd_opt(2023, 1, 1).unwrap();

        store.validate(today, false)?;

        Ok(store)
    }

    #[cfg(test)]
    pub fn mock_acquire(
        config: &str,
        audits: &str,
        imports: &str,
        today: chrono::NaiveDate,
        check_file_formatting: bool,
    ) -> Result<Self, StoreAcquireError> {
        let (config_src, config): (_, ConfigFile) = load_toml(CONFIG_TOML, config.as_bytes())?;
        let (audits_src, audits): (_, AuditsFile) = load_toml(AUDITS_TOML, audits.as_bytes())?;
        let (imports_src, imports): (_, ImportsFile) = load_toml(IMPORTS_LOCK, imports.as_bytes())?;

        let store = Self {
            lock: None,
            config,
            imports,
            audits,
            live_imports: None,
            config_src,
            audits_src,
            imports_src,
        };

        store.validate(today, check_file_formatting)?;

        Ok(store)
    }

    /// Create a clone of the store for use to resolve `suggest`.
    ///
    /// This cloned store will not contain `exemptions` entries from the config,
    /// unless they're marked as `suggest = false`, such that the resolver will
    /// identify these missing audits when generating a report.
    ///
    /// Unlike the primary store created with `Store::acquire` or
    /// `Store::create`, this store will not hold the store lock, and cannot be
    /// committed to disk by calling `commit()`.
    pub fn clone_for_suggest(&self) -> Self {
        let mut clone = Self {
            lock: None,
            config: self.config.clone(),
            imports: self.imports.clone(),
            audits: self.audits.clone(),
            live_imports: self.live_imports.clone(),
            config_src: self.config_src.clone(),
            audits_src: self.audits_src.clone(),
            imports_src: self.imports_src.clone(),
        };
        // Delete all exemptions entries except those that are suggest=false
        for versions in &mut clone.config.exemptions.values_mut() {
            versions.retain(|e| !e.suggest);
        }
        clone
    }

    /// Returns the set of audits which should be operated upon.
    ///
    /// If the store was acquired unlocked, this will include audits which are
    /// not stored in imports.lock, otherwise it will only contain imports
    /// stored locally.
    pub fn imported_audits(&self) -> &SortedMap<ImportName, AuditsFile> {
        match &self.live_imports {
            Some(live_imports) => &live_imports.audits,
            None => &self.imports.audits,
        }
    }

    /// Returns the set of publisher information which should be operated upon.
    ///
    /// If the store was acquired unlocked, whis may include publisher
    /// information which is not stored in imports.lock, otherwise it will only
    /// contain imports stored locally.
    pub fn publishers(&self) -> &SortedMap<PackageName, Vec<CratesPublisher>> {
        match &self.live_imports {
            Some(live_imports) => &live_imports.publisher,
            None => &self.imports.publisher,
        }
    }

    /// Commit the store's contents back to disk
    pub fn commit(self) -> Result<(), StoreCommitError> {
        // TODO: make this truly transactional?
        // (With a dir rename? Does that work with the lock? Fine because it's already closed?)
        if let Some(lock) = self.lock {
            let mut audits = lock.write_audits()?;
            let mut config = lock.write_config()?;
            let mut imports = lock.write_imports()?;
            audits.write_all(store_audits(self.audits)?.as_bytes())?;
            config.write_all(store_config(self.config)?.as_bytes())?;
            imports.write_all(store_imports(self.imports)?.as_bytes())?;
        }
        Ok(())
    }

    /// Mock `commit`. Returns the serialized value for each file in the store.
    /// Doesn't take `self` by value so that it can continue to be used.
    #[cfg(test)]
    pub fn mock_commit(&self) -> SortedMap<String, String> {
        [
            (
                AUDITS_TOML.to_owned(),
                store_audits(self.audits.clone()).unwrap(),
            ),
            (
                CONFIG_TOML.to_owned(),
                store_config(self.config.clone()).unwrap(),
            ),
            (
                IMPORTS_LOCK.to_owned(),
                store_imports(self.imports.clone()).unwrap(),
            ),
        ]
        .into_iter()
        .collect()
    }

    /// Validate the store's integrity
    #[allow(clippy::for_kv_map)]
    pub fn validate(
        &self,
        today: chrono::NaiveDate,
        check_file_formatting: bool,
    ) -> Result<(), StoreValidateErrors> {
        // ERRORS: ideally these are all gathered diagnostics, want to report as many errors
        // at once as possible!

        let max_end_date = today + chrono::Months::new(12);

        // TODO(#66): implement validation
        //
        // * check that policy entries are only first-party?
        //   * (we currently allow policy.criteria on third-parties for audit-as-crates-io)
        // * check that exemptions entries are for things that exist?
        // * check that lockfile and imports aren't desync'd (catch new/removed import urls)
        //
        // * check that each CriteriaEntry has 'description' or 'description_url'
        // * check that no one is trying to shadow builtin criteria (safe-to-run, safe-to-deploy)
        // * check that all 'audits' entries are well-formed
        // * check that all package names are valid (with crates.io...?)
        // * check that all reviews have a 'who' (currently an Option to stub it out)
        // * catch no-op deltas?
        // * nested check imports, complicated because of namespaces

        fn check_criteria(
            source_code: &SourceFile,
            valid: &Arc<Vec<CriteriaName>>,
            errors: &mut Vec<StoreValidateError>,
            criteria: &[Spanned<CriteriaName>],
        ) {
            for criteria in criteria {
                if !valid.contains(criteria) {
                    errors.push(StoreValidateError::InvalidCriteria(InvalidCriteriaError {
                        source_code: source_code.clone(),
                        span: Spanned::span(criteria),
                        invalid: criteria.to_string(),
                        valid_names: valid.clone(),
                    }))
                }
            }
        }

        let mut errors = Vec::new();

        // Fixme: this should probably be a Map...? Sorted? Stable?
        let valid_criteria = Arc::new(
            self.audits
                .criteria
                .keys()
                .map(|c| &**c)
                .chain([SAFE_TO_RUN, SAFE_TO_DEPLOY])
                .map(|name| name.to_string())
                .collect::<Vec<_>>(),
        );
        let no_criteria = vec![];

        for (_package, entries) in &self.config.exemptions {
            for entry in entries {
                check_criteria(
                    &self.config_src,
                    &valid_criteria,
                    &mut errors,
                    &entry.criteria,
                );
            }
        }
        for (_name, _version, policy) in &self.config.policy {
            check_criteria(
                &self.config_src,
                &valid_criteria,
                &mut errors,
                policy.criteria.as_ref().unwrap_or(&no_criteria),
            );
            check_criteria(
                &self.config_src,
                &valid_criteria,
                &mut errors,
                policy.dev_criteria.as_ref().unwrap_or(&no_criteria),
            );
            for (_dep_package, dep_criteria) in &policy.dependency_criteria {
                check_criteria(&self.config_src, &valid_criteria, &mut errors, dep_criteria);
            }
        }
        for (_new_criteria, entry) in &self.audits.criteria {
            // TODO: check that new_criteria isn't shadowing a builtin criteria
            check_criteria(
                &self.audits_src,
                &valid_criteria,
                &mut errors,
                &entry.implies,
            );
        }
        for (_package, entries) in &self.audits.audits {
            for entry in entries {
                // TODO: check that new_criteria isn't shadowing a builtin criteria
                check_criteria(
                    &self.audits_src,
                    &valid_criteria,
                    &mut errors,
                    &entry.criteria,
                );
            }
        }
        for (_package, entries) in &self.audits.wildcard_audits {
            for entry in entries {
                check_criteria(
                    &self.audits_src,
                    &valid_criteria,
                    &mut errors,
                    &entry.criteria,
                );

                if entry.end > max_end_date {
                    errors.push(StoreValidateError::BadWildcardEndDate(
                        BadWildcardEndDateError {
                            source_code: self.audits_src.clone(),
                            span: Spanned::span(&entry.end),
                            date: *entry.end,
                            max: max_end_date,
                        },
                    ))
                }
            }
        }

        // If requested, verify that files in the store are correctly formatted
        // and have no unrecognized fields. We don't want to be reformatting
        // them or dropping unused fields while in CI, as those changes will be
        // ignored.
        if check_file_formatting {
            for (name, old, new) in [
                (
                    CONFIG_TOML,
                    self.config_src.source(),
                    store_config(self.config.clone())
                        .unwrap_or_else(|_| self.config_src.source().to_owned()),
                ),
                (
                    AUDITS_TOML,
                    self.audits_src.source(),
                    store_audits(self.audits.clone())
                        .unwrap_or_else(|_| self.audits_src.source().to_owned()),
                ),
                (
                    IMPORTS_LOCK,
                    self.imports_src.source(),
                    store_imports(self.imports.clone())
                        .unwrap_or_else(|_| self.imports_src.source().to_owned()),
                ),
            ] {
                if old.trim_end() != new.trim_end() {
                    errors.push(StoreValidateError::BadFormat(BadFormatError {
                        unified_diff: unified_diff(
                            Algorithm::Myers,
                            old,
                            &new,
                            3,
                            Some((&format!("old/{name}"), &format!("new/{name}"))),
                        ),
                    }));
                }
            }
        }

        // If we're locked, and therefore not fetching new live imports,
        // validate that our imports.lock is in sync with config.toml.
        if self.imports_lock_outdated() {
            errors.push(StoreValidateError::ImportsLockOutdated);
        };

        if !errors.is_empty() {
            return Err(StoreValidateErrors { errors });
        }

        Ok(())
    }

    fn imports_lock_outdated(&self) -> bool {
        // If we have live imports, we're going to be updating imports.lock, so
        // it's OK if it's out-of-date with regard to the config.
        if self.live_imports.is_some() {
            return false;
        }

        // We must have the exact same set of imports, otherwise an import has
        // been added or removed and we're out of date.
        if self.config.imports.keys().ne(self.imports.audits.keys()) {
            return true;
        }

        for (import_name, config) in &self.config.imports {
            let audits_file = self.imports.audits.get(import_name).unwrap();
            // If we have any excluded crates in the imports.lock, it is out of
            // date and needs to be regenerated.
            for crate_name in &config.exclude {
                if audits_file.audits.contains_key(crate_name) {
                    return true;
                }
            }
        }

        false
    }

    /// Called to ensure that there is publisher information in the store's live
    /// imports for the given crate. This is used when adding new wildcard
    /// audits from `certify`.
    pub fn ensure_publisher_versions(
        &mut self,
        cfg: &Config,
        network: Option<&Network>,
        package: PackageStr<'_>,
    ) -> Result<&[CratesPublisher], CertifyError> {
        if let (Some(network), Some(live_imports)) = (network, self.live_imports.as_mut()) {
            let cache = Cache::acquire(cfg)?;
            import_publisher_versions(
                &cfg.metadata,
                network,
                &cache,
                &[package.to_owned()].into_iter().collect(),
                &self.config,
                &self.audits,
                &self.imports,
                live_imports,
            )?;

            Ok(live_imports
                .publisher
                .get(package)
                .map(|v| &v[..])
                .unwrap_or(&[]))
        } else {
            Ok(&[])
        }
    }
}

/// Process imported audits from the network, generating a `LiveImports`
/// description of the live state of imported audits.
fn process_imported_audits(
    fetched_audits: Vec<(ImportName, AuditsFile)>,
    local_audits_file: &AuditsFile,
    config_file: &ConfigFile,
    imports_lock: &ImportsFile,
    allow_criteria_changes: bool,
) -> Result<ImportsFile, CriteriaChangeErrors> {
    let mut new_imports = ImportsFile {
        publisher: SortedMap::new(),
        audits: SortedMap::new(),
    };
    let mut changed_criteria = Vec::new();

    let local_criteria_mapper = CriteriaMapper::new(&local_audits_file.criteria);
    for (import_name, mut audits_file) in fetched_audits {
        let config = config_file
            .imports
            .get(&import_name)
            .expect("fetched audit without config?");

        // Remove any excluded audits from the live copy. We'll effectively
        // pretend they don't exist upstream.
        for excluded in &config.exclude {
            audits_file.audits.remove(excluded);
        }

        // Construct a mapping from the foreign criteria namespace into the
        // local criteria namespace based on the criteria map from the config.
        let foreign_criteria_mapper = CriteriaMapper::new(&audits_file.criteria);
        let foreign_to_local_mapping: Vec<_> = foreign_criteria_mapper
            .all_criteria_names()
            .map(|foreign_name| {
                // NOTE: We try the map before we check for built-in criteria to
                // allow overriding the default behaviour.
                if let Some(mapped) = config.criteria_map.get(foreign_name) {
                    local_criteria_mapper.criteria_from_list(mapped)
                } else if foreign_name == SAFE_TO_DEPLOY {
                    local_criteria_mapper.criteria_from_list([SAFE_TO_DEPLOY])
                } else if foreign_name == SAFE_TO_RUN {
                    local_criteria_mapper.criteria_from_list([SAFE_TO_RUN])
                } else {
                    local_criteria_mapper.no_criteria()
                }
            })
            .collect();

        // Helper to re-write foreign criteria into the local criteria
        // namespace.
        let make_criteria_local = |criteria: &mut Vec<Spanned<CriteriaName>>| {
            let foreign_set = foreign_criteria_mapper.criteria_from_list(&*criteria);
            let mut local_set = local_criteria_mapper.no_criteria();
            for foreign_criteria_idx in foreign_set.indices() {
                local_set.unioned_with(&foreign_to_local_mapping[foreign_criteria_idx]);
            }
            *criteria = local_criteria_mapper
                .criteria_names(&local_set)
                .map(|name| name.to_owned().into())
                .collect();
        };

        // By default all audits read from the network are fresh.
        //
        // Note: This may leave behind useless audits which imply no criteria,
        // but that's OK - we'll never choose to import them. In the future we
        // might want to trim them.
        for audit_entry in audits_file.audits.values_mut().flat_map(|v| v.iter_mut()) {
            audit_entry.is_fresh_import = true;
            make_criteria_local(&mut audit_entry.criteria);
        }
        for audit_entry in audits_file
            .wildcard_audits
            .values_mut()
            .flat_map(|v| v.iter_mut())
        {
            audit_entry.is_fresh_import = true;
            make_criteria_local(&mut audit_entry.criteria);
        }

        // Now that we're done with foreign criteria, trim the set to only
        // contain mapped criteria, as we don't care about other criteria, so
        // shouldn't bother importing them.
        //
        // We also clear out description_url and implies, as those fields will
        // not be used locally.
        audits_file.criteria.retain(|name, entry| {
            entry.description_url = None;
            entry.implies = Vec::new();
            config.criteria_map.contains_key(name)
        });

        // If we have an existing audits file for these imports, compare against it.
        if let Some(existing_audits_file) = imports_lock.audits.get(&import_name) {
            if !allow_criteria_changes {
                // Compare the new criteria descriptions with existing criteria
                // descriptions. If the description already exists, record a
                // CriteriaChangeError.
                for (criteria_name, old_entry) in &existing_audits_file.criteria {
                    if let Some(new_entry) = audits_file.criteria.get(criteria_name) {
                        let old_desc = old_entry.description.as_ref().unwrap();
                        let new_desc = new_entry.description.as_ref().unwrap();
                        if old_desc != new_desc {
                            changed_criteria.push(CriteriaChangeError {
                                import_name: import_name.clone(),
                                criteria_name: criteria_name.to_owned(),
                                unified_diff: unified_diff(
                                    Algorithm::Myers,
                                    old_desc,
                                    new_desc,
                                    5,
                                    None,
                                ),
                            });
                        }
                    }
                }
            }

            // Compare the new audits with existing audits. If an audit already
            // existed in the existing audits file, mark it as non-fresh.
            for (package, existing_audits) in &existing_audits_file.audits {
                let new_audits = audits_file
                    .audits
                    .get_mut(package)
                    .map(|v| &mut v[..])
                    .unwrap_or(&mut []);
                for existing_audit in existing_audits {
                    for new_audit in &mut *new_audits {
                        if new_audit.is_fresh_import && new_audit.same_audit_as(existing_audit) {
                            new_audit.is_fresh_import = false;
                            break;
                        }
                    }
                }
            }
            for (package, existing_audits) in &existing_audits_file.wildcard_audits {
                let new_audits = audits_file
                    .wildcard_audits
                    .get_mut(package)
                    .map(|v| &mut v[..])
                    .unwrap_or(&mut []);
                for existing_audit in existing_audits {
                    for new_audit in &mut *new_audits {
                        if new_audit.is_fresh_import && new_audit.same_audit_as(existing_audit) {
                            new_audit.is_fresh_import = false;
                            break;
                        }
                    }
                }
            }
        }

        // Now add the new import
        new_imports.audits.insert(import_name, audits_file);
    }

    if !changed_criteria.is_empty() {
        return Err(CriteriaChangeErrors {
            errors: changed_criteria,
        });
    }

    // FIXME: Consider doing some additional validation on these audits
    // before returning?

    Ok(new_imports)
}

/// Fetch all declared imports from the network, filling in any criteria
/// descriptions.
async fn fetch_imported_audits(
    network: &Network,
    config: &ConfigFile,
) -> Result<Vec<(ImportName, AuditsFile)>, Box<FetchAuditError>> {
    let progress_bar = progress_bar("Fetching", "imported audits", config.imports.len() as u64);
    try_join_all(config.imports.iter().map(|(name, import)| async {
        let _guard = IncProgressOnDrop(&progress_bar, 1);
        let audit_file = fetch_imported_audit(network, name, &import.url)
            .await
            .map_err(Box::new)?;
        Ok::<_, Box<FetchAuditError>>((name.clone(), audit_file))
    }))
    .await
}

/// Fetch a single AuditsFile from the network, filling in any criteria
/// descriptions.
async fn fetch_imported_audit(
    network: &Network,
    name: &str,
    url: &str,
) -> Result<AuditsFile, FetchAuditError> {
    let parsed_url = Url::parse(url).map_err(|error| FetchAuditError::InvalidUrl {
        import_url: url.to_owned(),
        import_name: name.to_owned(),
        error,
    })?;
    let audit_bytes = network.download(parsed_url).await?;
    let audit_string = String::from_utf8(audit_bytes).map_err(LoadTomlError::from)?;
    let audit_source = SourceFile::new(name, audit_string.clone());

    // Attempt to parse each criteria and audit independently, to allow
    // recovering from parsing or validation errors on a per-entry basis when
    // importing audits. This reduces the risk of an upstream vendor adopting a
    // new cargo-vet feature breaking projects still using an older version of
    // cargo-vet.
    let foreign_audit_file: ForeignAuditsFile = toml::de::from_str(&audit_string)
        .map_err(|error| {
            let (line, col) = error.line_col().unwrap_or((0, 0));
            TomlParseError {
                source_code: audit_source,
                span: SourceOffset::from_location(&audit_string, line + 1, col + 1),
                error,
            }
        })
        .map_err(LoadTomlError::from)?;
    let ForeignAuditFileToLocalResult {
        mut audit_file,
        ignored_criteria,
        ignored_audits,
    } = foreign_audit_file_to_local(foreign_audit_file);
    if !ignored_criteria.is_empty() {
        warn!(
            "Ignored {} invalid criteria entries when importing from '{}'\n\
            These criteria may have been made with a more recent version of cargo-vet",
            ignored_criteria.len(),
            name
        );
        info!(
            "The following criteria were ignored when importing from '{}': {:?}",
            name, ignored_criteria
        );
    }
    if !ignored_audits.is_empty() {
        warn!(
            "Ignored {} invalid audits when importing from '{}'\n\
            These audits may have been made with a more recent version of cargo-vet",
            ignored_audits.len(),
            name
        );
        info!(
            "Audits for the following packages were ignored when importing from '{}': {:?}",
            name, ignored_audits
        );
    }

    // Eagerly fetch all descriptions for criteria in the imported audits file,
    // and store them inline. We'll error out if any of these descriptions are
    // unavailable.
    try_join_all(
        audit_file
            .criteria
            .iter_mut()
            .map(|(criteria_name, criteria_entry)| async {
                if criteria_entry.description.is_some() {
                    return Ok(());
                }

                let url_string = criteria_entry.description_url.as_ref().ok_or_else(|| {
                    FetchAuditError::MissingCriteriaDescription {
                        import_name: name.to_owned(),
                        criteria_name: criteria_name.clone(),
                    }
                })?;
                let url = Url::parse(url_string).map_err(|error| {
                    FetchAuditError::InvalidCriteriaDescriptionUrl {
                        import_name: name.to_owned(),
                        criteria_name: criteria_name.clone(),
                        url: url_string.clone(),
                        error,
                    }
                })?;
                let bytes = network.download(url.clone()).await?;
                let description =
                    String::from_utf8(bytes).map_err(|error| DownloadError::InvalidText {
                        url: Box::new(url.clone()),
                        error,
                    })?;

                criteria_entry.description = Some(description);
                Ok::<(), FetchAuditError>(())
            }),
    )
    .await?;

    Ok(audit_file)
}

pub(crate) struct ForeignAuditFileToLocalResult {
    pub audit_file: AuditsFile,
    pub ignored_criteria: Vec<CriteriaName>,
    pub ignored_audits: Vec<PackageName>,
}

fn is_known_criteria(valid_criteria: &[CriteriaName], criteria_name: &CriteriaName) -> bool {
    criteria_name == format::SAFE_TO_RUN
        || criteria_name == format::SAFE_TO_DEPLOY
        || valid_criteria.contains(criteria_name)
}

/// Convert a foreign audits file into a local audits file, ignoring any entries
/// which could not be interpreted, due to issues such as being created with a
/// newer version of cargo-vet.
pub(crate) fn foreign_audit_file_to_local(
    foreign_audit_file: ForeignAuditsFile,
) -> ForeignAuditFileToLocalResult {
    let mut ignored_criteria = Vec::new();
    let mut criteria: SortedMap<CriteriaName, CriteriaEntry> = foreign_audit_file
        .criteria
        .into_iter()
        .filter_map(|(criteria, value)| match parse_imported_criteria(value) {
            Some(entry) => Some((criteria, entry)),
            None => {
                ignored_criteria.push(criteria);
                None
            }
        })
        .collect();
    let valid_criteria: Vec<CriteriaName> = criteria.keys().cloned().collect();

    // Remove any unknown criteria from implies sets, to ensure we don't run
    // into errors later on in the resolver.
    for entry in criteria.values_mut() {
        entry
            .implies
            .retain(|criteria_name| is_known_criteria(&valid_criteria, criteria_name));
    }

    let mut ignored_audits = Vec::new();
    let audits: AuditedDependencies = foreign_audit_file
        .audits
        .into_iter()
        .map(|(package, audits)| {
            let parsed: Vec<_> = audits
                .into_iter()
                .filter_map(|value| match parse_imported_audit(&valid_criteria, value) {
                    Some(audit) => Some(audit),
                    None => {
                        ignored_audits.push(package.clone());
                        None
                    }
                })
                .collect();
            (package, parsed)
        })
        .filter(|(_, audits)| !audits.is_empty())
        .collect();

    let wildcard_audits: WildcardAudits = foreign_audit_file
        .wildcard_audits
        .into_iter()
        .map(|(package, audits)| {
            let parsed: Vec<_> = audits
                .into_iter()
                .filter_map(
                    |value| match parse_imported_wildcard_audit(&valid_criteria, value) {
                        Some(audit) => Some(audit),
                        None => {
                            ignored_audits.push(package.clone());
                            None
                        }
                    },
                )
                .collect();
            (package, parsed)
        })
        .filter(|(_, audits)| !audits.is_empty())
        .collect();

    ForeignAuditFileToLocalResult {
        audit_file: AuditsFile {
            criteria,
            wildcard_audits,
            audits,
        },
        ignored_criteria,
        ignored_audits,
    }
}

/// Parse an unparsed criteria entry, validating and returning it.
fn parse_imported_criteria(value: toml::Value) -> Option<CriteriaEntry> {
    parse_from_value(value)
        .map_err(|err| info!("imported criteria parsing failed due to {err}"))
        .ok()
}

/// Parse an unparsed audit entry, validating and returning it.
fn parse_imported_audit(valid_criteria: &[CriteriaName], value: toml::Value) -> Option<AuditEntry> {
    let mut audit: AuditEntry = parse_from_value(value)
        .map_err(|err| info!("imported audit parsing failed due to {err}"))
        .ok()?;

    // Remove any unrecognized criteria to avoid later errors caused by being
    // unable to find criteria, and ignore the entry if it names no known
    // criteria.
    audit
        .criteria
        .retain(|criteria_name| is_known_criteria(valid_criteria, criteria_name));
    if audit.criteria.is_empty() {
        info!("imported audit parsing failed due to no known criteria");
        return None;
    }

    Some(audit)
}

/// Parse an unparsed wildcard audit entry, validating and returning it.
fn parse_imported_wildcard_audit(
    valid_criteria: &[CriteriaName],
    value: toml::Value,
) -> Option<WildcardEntry> {
    let mut audit: WildcardEntry = parse_from_value(value)
        .map_err(|err| info!("imported wildcard audit parsing failed due to {err}"))
        .ok()?;

    audit
        .criteria
        .retain(|criteria_name| is_known_criteria(valid_criteria, criteria_name));
    if audit.criteria.is_empty() {
        info!("imported wildcard audit parsing failed due to no known criteria");
        return None;
    }

    Some(audit)
}

fn wildcard_audits_packages(
    audits_file: &AuditsFile,
    imports_file: &ImportsFile,
) -> FastSet<PackageName> {
    // Determine which versions are relevant for the purposes of wildcard audit
    // checks. We'll only care about crates which have associated wildcard
    // audits or existing cached publisher info.
    audits_file
        .wildcard_audits
        .keys()
        .chain(
            imports_file
                .audits
                .values()
                .flat_map(|audits_file| audits_file.wildcard_audits.keys()),
        )
        .chain(imports_file.publisher.keys())
        .cloned()
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn import_publisher_versions(
    metadata: &cargo_metadata::Metadata,
    network: &Network,
    cache: &Cache,
    relevant_packages: &FastSet<PackageName>,
    config_file: &ConfigFile,
    audits_file: &AuditsFile,
    imports_lock: &ImportsFile,
    live_imports: &mut ImportsFile,
) -> Result<(), FetchAuditError> {
    // We also only care about versions for third-party packages which are
    // actually used in-tree.
    let mut relevant_versions: FastMap<PackageStr<'_>, FastSet<&semver::Version>> = FastMap::new();
    for pkg in &metadata.packages {
        if relevant_packages.contains(&pkg.name) && pkg.is_third_party(&config_file.policy) {
            relevant_versions
                .entry(&pkg.name)
                .or_default()
                .insert(&pkg.version);
        }
    }

    // We may care about other versions for these packages as well if they
    // appear on the `from` side of a delta audit, or if they are named by an
    // exemption.
    let mut relevant_publishers = Vec::new();
    for (pkg_name, mut versions) in relevant_versions {
        // If there is a delta audit originating from a specific version, that
        // version may be relevant, so record it.
        for audit in [audits_file]
            .into_iter()
            .chain(live_imports.audits.values())
            .flat_map(|audits_file| audits_file.audits.get(pkg_name))
            .flatten()
        {
            if let AuditKind::Delta { from, .. } = &audit.kind {
                versions.insert(&from.semver);
            }
        }

        // If there is an exemption naming a specific version, it is also
        // potentially relevant.
        if let Some(exemptions) = config_file.exemptions.get(pkg_name) {
            for exemption in exemptions {
                versions.insert(&exemption.version.semver);
            }
        }

        // If we have previously cached publisher information, it's relevant.
        if let Some(publishers) = imports_lock.publisher.get(pkg_name) {
            for publisher in publishers {
                versions.insert(&publisher.version.semver);
            }
        }

        // Access the set of publishers. We provide the set of relevant versions
        // to help decide whether or not to fetch new publisher information from
        // crates.io, to reduce API activity.
        let publishers = cache.get_publishers(network, pkg_name, versions)?;
        relevant_publishers.push((pkg_name, publishers));
    }

    // NOTE: We make sure to process all imports before we look up user
    // information in the cache, to ensure we're fetching consistent user
    // information.
    for (pkg_name, publishers) in relevant_publishers {
        // Fill in the live imports table with the relevant information.
        let nonfresh_versions: FastSet<_> = imports_lock
            .publisher
            .get(pkg_name)
            .into_iter()
            .flatten()
            .map(|publisher| &publisher.version.semver)
            .collect();

        live_imports.publisher.insert(
            pkg_name.to_owned(),
            publishers
                .into_iter()
                .filter_map(|crates_version| {
                    let user_id = crates_version.published_by?;
                    let user_info = cache.get_crates_user_info(user_id)?;
                    let is_fresh_import = !nonfresh_versions.contains(&crates_version.num);
                    Some(CratesPublisher {
                        version: VetVersion {
                            semver: crates_version.num,
                            git_rev: None,
                        },
                        user_id,
                        user_login: user_info.login,
                        user_name: user_info.name,
                        when: crates_version.created_at.date_naive(),
                        is_fresh_import,
                    })
                })
                .collect(),
        );
    }

    Ok(())
}

/// A Registry in CARGO_HOME (usually the crates.io one)
pub struct CargoRegistry {
    /// The queryable index
    index: CratesIndex,
    /// The base path all registries share (`$CARGO_HOME/registry`)
    base_dir: PathBuf,
    /// The name of the registry (`github.com-1ecc6299db9ec823`)
    registry: OsString,
    /// Whether or not the index is known to be up-to-date
    index_up_to_date: bool,
}

impl CargoRegistry {
    /// Get the src dir of this registry (unpacked fetches)
    pub fn src(&self) -> PathBuf {
        self.base_dir.join(CARGO_REGISTRY_SRC).join(&self.registry)
    }
    /// Get the cache dir of the registry (.crate packed fetches)
    pub fn cache(&self) -> PathBuf {
        self.base_dir
            .join(CARGO_REGISTRY_CACHE)
            .join(&self.registry)
    }
    // Could also include the index, not reason to do that yet
}

struct CacheState {
    /// The loaded DiffCache, will be written back on Drop
    diff_cache: DiffCache,
    /// Command history to provide some persistent magic smarts
    command_history: CommandHistory,
    /// Cache of fetched info from crates.io about who published which versions of crates.
    publisher_cache: PublisherCache,
    /// Paths for unpacked packages from this version.
    fetched_packages: FastMap<(String, VetVersion), Arc<tokio::sync::OnceCell<PathBuf>>>,
    /// Computed diffstats from this version.
    diffed: FastMap<(String, Delta), Arc<tokio::sync::OnceCell<DiffStat>>>,
}

/// The cache where we store globally shared artifacts like fetched packages and diffstats
///
/// All access to this directory should be managed by this type to avoid races.
pub struct Cache {
    /// System-global lock over the cache, will be None if we're mocking.
    _lock: Option<FileLock>,
    /// Path to the root of the cache
    root: Option<PathBuf>,
    /// Cargo's crates.io package registry (in CARGO_HOME) for us to query opportunistically
    cargo_registry: Option<CargoRegistry>,
    /// Path to the DiffCache (for when we want to save it back)
    diff_cache_path: Option<PathBuf>,
    /// Path to the CommandHistory (for when we want to save it back)
    command_history_path: Option<PathBuf>,
    /// Path to the PublisherCache (for when we want to save it back)
    publisher_cache_path: Option<PathBuf>,
    /// Semaphore preventing exceeding the maximum number of concurrent diffs.
    diff_semaphore: tokio::sync::Semaphore,
    /// Common mutable state for the cache which can be mutated concurrently
    /// from multiple tasks.
    state: Mutex<CacheState>,
}

impl Drop for Cache {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap();
        if let Some(diff_cache_path) = &self.diff_cache_path {
            // Write back the diff_cache
            if let Err(err) = || -> Result<(), CacheCommitError> {
                let diff_cache = store_diff_cache(mem::take(&mut state.diff_cache))?;
                fs::write(diff_cache_path, diff_cache)?;
                Ok(())
            }() {
                error!("error writing back changes to diff-cache: {:?}", err);
            }
        }
        if let Some(command_history_path) = &self.command_history_path {
            // Write back the command_history
            if let Err(err) = || -> Result<(), CacheCommitError> {
                let command_history = store_command_history(mem::take(&mut state.command_history))?;
                fs::write(command_history_path, command_history)?;
                Ok(())
            }() {
                error!("error writing back changes to command history: {:?}", err);
            }
        }
        if let Some(publisher_cache_path) = &self.publisher_cache_path {
            // Write back the publisher_cache
            if let Err(err) = || -> Result<(), CacheCommitError> {
                let publisher_cache = store_publisher_cache(mem::take(&mut state.publisher_cache))?;
                fs::write(publisher_cache_path, publisher_cache)?;
                Ok(())
            }() {
                error!("error writing back changes to publisher-cache: {:?}", err);
            }
        }
        // `_lock: FileLock` implicitly released here
    }
}

impl Cache {
    /// Acquire the cache
    pub fn acquire(cfg: &PartialConfig) -> Result<Self, CacheAcquireError> {
        // Try to get the cargo registry
        let cargo_registry = find_cargo_registry();
        if let Err(e) = &cargo_registry {
            // ERRORS: this warning really rides the line, I'm not sure if the user can/should care
            warn!("Couldn't find cargo registry: {e}");
        }

        if cfg.mock_cache {
            // We're in unit tests, everything should be mocked and not touch real caches
            return Ok(Cache {
                _lock: None,
                root: None,
                cargo_registry: cargo_registry.ok(),
                diff_cache_path: None,
                command_history_path: None,
                publisher_cache_path: None,
                diff_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_DIFFS),
                state: Mutex::new(CacheState {
                    diff_cache: DiffCache::default(),
                    command_history: CommandHistory::default(),
                    publisher_cache: PublisherCache::default(),
                    fetched_packages: FastMap::new(),
                    diffed: FastMap::new(),
                }),
            });
        }

        // Make sure the cache directory exists, and acquire an exclusive lock on it.
        let root = cfg.cache_dir.clone();
        fs::create_dir_all(&root).map_err(|error| CacheAcquireError::Root {
            target: root.clone(),
            error,
        })?;

        let lock = Filesystem::new(root.clone()).open_rw(CACHE_VET_LOCK, "cache lock")?;

        let empty = root.join(CACHE_EMPTY_PACKAGE);
        fs::create_dir_all(&empty).map_err(|error| CacheAcquireError::Empty {
            target: empty.clone(),
            error,
        })?;

        let packages_src = root.join(CACHE_REGISTRY_SRC);
        fs::create_dir_all(&packages_src).map_err(|error| CacheAcquireError::Src {
            target: packages_src.clone(),
            error,
        })?;

        let packages_cache = root.join(CACHE_REGISTRY_CACHE);
        fs::create_dir_all(&packages_cache).map_err(|error| CacheAcquireError::Cache {
            target: packages_cache.clone(),
            error,
        })?;

        // Setup the diff_cache.
        let diff_cache_path = root.join(CACHE_DIFF_CACHE);
        let diff_cache: DiffCache = File::open(&diff_cache_path)
            .ok()
            .and_then(|f| load_toml(CACHE_DIFF_CACHE, f).map(|v| v.1).ok())
            .unwrap_or_default();

        // Setup the command_history.
        let command_history_path = root.join(CACHE_COMMAND_HISTORY);
        let command_history: CommandHistory = File::open(&command_history_path)
            .ok()
            .and_then(|f| load_json(f).ok())
            .unwrap_or_default();

        // Setup the publisher_cache.
        let publisher_cache_path = root.join(CACHE_PUBLISHER_CACHE);
        let publisher_cache: PublisherCache = File::open(&publisher_cache_path)
            .ok()
            .and_then(|f| load_json(f).ok())
            .unwrap_or_default();

        Ok(Self {
            _lock: Some(lock),
            root: Some(root),
            diff_cache_path: Some(diff_cache_path),
            command_history_path: Some(command_history_path),
            publisher_cache_path: Some(publisher_cache_path),
            cargo_registry: cargo_registry.ok(),
            diff_semaphore: tokio::sync::Semaphore::new(MAX_CONCURRENT_DIFFS),
            state: Mutex::new(CacheState {
                diff_cache,
                command_history,
                publisher_cache,
                fetched_packages: FastMap::new(),
                diffed: FastMap::new(),
            }),
        })
    }

    /// Check if the Cache has access to the registry or information about the
    /// crates.io index.
    pub fn has_registry(&self) -> bool {
        self.cargo_registry.is_some()
    }

    /// Ensures that the local copy of the crates.io index has the most
    /// up-to-date information about what crates are available.
    ///
    /// Returns `true` if the state of the index may have been changed by this
    /// call, and `false` if the index is already up-to-date.
    pub fn ensure_index_up_to_date(&mut self) -> bool {
        let reg = match &mut self.cargo_registry {
            Some(reg) => reg,
            None => return false,
        };
        if reg.index_up_to_date {
            return false;
        }
        let _spinner = indeterminate_spinner("Updating", "registry index");
        reg.index_up_to_date = true;
        match reg.index.update() {
            Ok(()) => true,
            Err(e) => {
                warn!("Couldn't update cargo index: {e}");
                false
            }
        }
    }

    /// Gets any information the crates.io index has on this package, locally
    /// with no downloads. The index may be out of date, however a caller can
    /// use `ensure_index_up_to_date` to make sure it is up to date before
    /// calling this method.
    ///
    /// However this may do some expensive disk i/o, so ideally we should do
    /// some bulk processing of this later. For now let's get it working...
    pub fn query_package_from_index(&self, name: PackageStr) -> Option<crates_index::Crate> {
        let reg = self.cargo_registry.as_ref()?;
        reg.index.crate_(name)
    }

    #[tracing::instrument(skip(self, metadata, network), err)]
    pub async fn fetch_package(
        &self,
        metadata: &cargo_metadata::Metadata,
        network: Option<&Network>,
        package: PackageStr<'_>,
        version: &VetVersion,
    ) -> Result<PathBuf, FetchError> {
        // Lock the mutex to extract a reference to the OnceCell which we'll use
        // to asynchronously synchronize on and fetch the package only once in a
        // single execution.
        let once_cell = {
            // NOTE: Don't .await while this is held, or we might deadlock!
            let mut guard = self.state.lock().unwrap();
            guard
                .fetched_packages
                .entry((package.to_owned(), version.clone()))
                .or_default()
                .clone()
        };

        let path_res: Result<_, FetchError> = once_cell
            .get_or_try_init(|| async {
                let root = self.root.as_ref().unwrap();

                // crates.io won't have a copy of any crates with git revision
                // versions, so they need to be found in local clones within the
                // cargo metadata, otherwise we cannot find them.
                if let Some(git_rev) = &version.git_rev {
                    let repacked_src = root.join(CACHE_REGISTRY_SRC).join(format!(
                        "{}-{}.git.{}",
                        package,
                        version.semver,
                        version.git_rev.as_ref().unwrap()
                    ));
                    if fetch_is_ok(&repacked_src).await {
                        return Ok(repacked_src);
                    }

                    // We don't have a cached re-pack - repack again ourselves.
                    let checkout_path = locate_local_checkout(metadata, package, version)
                        .ok_or_else(|| FetchError::UnknownGitRevision {
                            package: package.to_owned(),
                            git_rev: git_rev.to_owned(),
                        })?;

                    // We re-package any git checkouts into the cache in order
                    // to maintain a consistent directory structure with crates
                    // fetched from crates.io in diffs.
                    unpack_checkout(&checkout_path, &repacked_src)
                        .await
                        .map_err(|error| FetchError::UnpackCheckout {
                            src: checkout_path,
                            error,
                        })?;
                    return Ok(repacked_src);
                }

                let version = &version.semver;

                let dir_name = format!("{package}-{version}");

                // First try to get a cached copy from cargo's registry.
                if let Some(reg) = self.cargo_registry.as_ref() {
                    let fetched_src = reg.src().join(&dir_name);
                    if fetch_is_ok(&fetched_src).await {
                        return Ok(fetched_src);
                    }
                }

                // Paths for the fetched package and checkout in our local cache.
                let fetched_package = root
                    .join(CACHE_REGISTRY_CACHE)
                    .join(format!("{dir_name}.crate"));
                let fetched_src = root.join(CACHE_REGISTRY_SRC).join(&dir_name);

                // Check if the resource is already available in our local cache.
                let fetched_package_ = fetched_package.clone();
                let cached_file = tokio::task::spawn_blocking(move || {
                    File::open(&fetched_package_).map(|file| {
                        // Update the atime and mtime for this crate to ensure it isn't
                        // collected by the gc.
                        let now = filetime::FileTime::now();
                        if let Err(err) =
                            filetime::set_file_handle_times(&file, Some(now), Some(now))
                        {
                            warn!(
                                "failed to update mtime for {}, gc may not function correctly: {}",
                                fetched_package_.display(),
                                err
                            );
                        }
                        file
                    })
                })
                .await
                .expect("failed to join");

                // If the file isn't in our local cache, make sure to download it.
                let file = match cached_file {
                    Ok(file) => file,
                    Err(_) => {
                        let network = network.ok_or_else(|| FetchError::Frozen {
                            package: package.to_owned(),
                            version: version.clone(),
                        })?;

                        // We don't have it, so download it
                        let url =
                            format!("https://crates.io/api/v1/crates/{package}/{version}/download");
                        let url = Url::parse(&url).map_err(|error| FetchError::InvalidUrl {
                            url: url.clone(),
                            error,
                        })?;
                        info!(
                            "downloading package {}:{} from {} to {}",
                            package,
                            version,
                            url,
                            fetched_package.display()
                        );
                        network.download_and_persist(url, &fetched_package).await?;

                        let fetched_package_ = fetched_package.clone();
                        tokio::task::spawn_blocking(move || File::open(fetched_package_))
                            .await
                            .expect("failed to join")
                            .map_err(|error| FetchError::OpenCached {
                                target: fetched_package.clone(),
                                error,
                            })?
                    }
                };

                // TODO(#116): take the SHA2 of the bytes and compare it to what the registry says

                if fetch_is_ok(&fetched_src).await {
                    Ok(fetched_src)
                } else {
                    info!(
                        "unpacking package {}:{} from {} to {}",
                        package,
                        version,
                        fetched_package.display(),
                        fetched_src.display()
                    );
                    // The tarball needs to be unpacked, so do so.
                    tokio::task::spawn_blocking(move || {
                        unpack_package(&file, &fetched_src)
                            .map(|_| fetched_src)
                            .map_err(|error| FetchError::Unpack {
                                src: fetched_package.clone(),
                                error,
                            })
                    })
                    .await
                    .expect("failed to join")
                }
            })
            .await;
        let path = path_res?;
        Ok(path.to_owned())
    }

    #[tracing::instrument(skip_all, err)]
    pub async fn diffstat_package(
        &self,
        version1: &Path,
        version2: &Path,
        has_git_rev: bool,
    ) -> Result<(DiffStat, Vec<(PathBuf, PathBuf)>), DiffError> {
        let _permit = self
            .diff_semaphore
            .acquire()
            .await
            .expect("Semaphore dropped?!");

        // ERRORS: all of this is properly fallible internal workings, we can fail
        // to diffstat some packages and still produce some useful output
        trace!("diffstating {version1:#?} {version2:#?}");

        let out = tokio::process::Command::new("git")
            .arg("diff")
            .arg("--ignore-cr-at-eol")
            .arg("--no-index")
            .arg("--numstat")
            .arg("-z")
            .arg(version1)
            .arg(version2)
            .output()
            .await
            .map_err(CommandError::CommandFailed)?;

        let status = out.status.code().unwrap_or(-1);
        // 0 = empty
        // 1 = some diff
        if status != 0 && status != 1 {
            return Err(CommandError::BadStatus(status).into());
        }

        let mut diffstat = DiffStat {
            files_changed: 0,
            insertions: 0,
            deletions: 0,
        };
        let mut to_compare = Vec::new();

        // Thanks to the `-z` flag the output takes the rough format of:
        // "{INSERTED}\t{DELETED}\t\0{FROM_PATH}\0{TO_PATH}\0" for each file
        // being diffed. If the file was added or removed one of the sides will
        // be "/dev/null", even on Windows. Binary files use "-" for the
        // inserted & deleted counts.
        let output = String::from_utf8(out.stdout).map_err(CommandError::BadOutput)?;
        let mut chunks = output.split('\0');
        while let (Some(changes_s), Some(from_s), Some(to_s)) =
            (chunks.next(), chunks.next(), chunks.next())
        {
            // Check if the path is one of the files which is ignored.
            let rel_path = if to_s != "/dev/null" {
                Path::new(to_s)
                    .strip_prefix(version2)
                    .map_err(DiffError::UnexpectedPath)?
            } else {
                assert_ne!(
                    from_s, "/dev/null",
                    "unexpected diff from /dev/null to /dev/null"
                );
                Path::new(from_s)
                    .strip_prefix(version1)
                    .map_err(DiffError::UnexpectedPath)?
            };
            if DIFF_SKIP_PATHS.iter().any(|p| Path::new(p) == rel_path)
                || (has_git_rev && Path::new(CARGO_TOML_FILE) == rel_path)
            {
                continue;
            }

            to_compare.push((from_s.into(), to_s.into()));

            diffstat.files_changed += 1;

            match changes_s.trim().split_once('\t') {
                Some(("-", "-")) => {} // binary diff
                Some((insertions_s, deletions_s)) => {
                    diffstat.insertions += insertions_s
                        .parse::<u64>()
                        .map_err(|_| DiffError::InvalidOutput)?;
                    diffstat.deletions += deletions_s
                        .parse::<u64>()
                        .map_err(|_| DiffError::InvalidOutput)?;
                }
                None => Err(DiffError::InvalidOutput)?,
            };
        }
        Ok((diffstat, to_compare))
    }

    #[tracing::instrument(skip(self, metadata, network), err)]
    pub async fn fetch_and_diffstat_package(
        &self,
        metadata: &cargo_metadata::Metadata,
        network: Option<&Network>,
        package: PackageStr<'_>,
        delta: &Delta,
    ) -> Result<DiffStat, FetchAndDiffError> {
        // Lock the mutex to extract a reference to the OnceCell which we'll use
        // to asynchronously synchronize on and diff the package only once in a
        // single execution.
        //
        // While we have the mutex locked, we'll also check the DiffStat cache
        // to return without any async steps if possible.
        let once_cell = {
            // NOTE: Don't .await while this is held, or we might deadlock!
            let mut guard = self.state.lock().unwrap();

            // Check if the value has already been cached.
            let DiffCache::V2 { diffs } = &guard.diff_cache;
            if let Some(cached) = diffs
                .get(package)
                .and_then(|cache| cache.get(delta))
                .cloned()
            {
                return Ok(cached);
            }

            if self.root.is_none() {
                // If we don't have a root, assume we want mocked results
                // ERRORS: this warning really rides the line, I'm not sure if the user can/should care
                warn!("Missing root, assuming we're in tests and mocking");

                let from_len = match &delta.from {
                    Some(from) => from.semver.major * from.semver.major,
                    None => 0,
                };
                let to_len: u64 = delta.to.semver.major * delta.to.semver.major;
                let diff = to_len as i64 - from_len as i64;
                let count = diff.unsigned_abs();
                return Ok(DiffStat {
                    files_changed: 1,
                    insertions: if diff > 0 { count } else { 0 },
                    deletions: if diff < 0 { count } else { 0 },
                });
            }

            guard
                .diffed
                .entry((package.to_owned(), delta.clone()))
                .or_default()
                .clone()
        };

        let diffstat = once_cell
            .get_or_try_init(|| async {
                let from = match &delta.from {
                    Some(from) => self.fetch_package(metadata, network, package, from).await?,
                    None => self.root.as_ref().unwrap().join(CACHE_EMPTY_PACKAGE),
                };
                let to = self
                    .fetch_package(metadata, network, package, &delta.to)
                    .await?;

                // Have fetches, do a real diffstat
                // NOTE: We'll never pick a 'from' version with a git_rev, so we
                // don't need to check for that here.
                let (diffstat, _) = self
                    .diffstat_package(&from, &to, delta.to.git_rev.is_some())
                    .await?;

                // Record the cache result in the diffcache
                {
                    let mut guard = self.state.lock().unwrap();
                    let DiffCache::V2 { diffs } = &mut guard.diff_cache;
                    diffs
                        .entry(package.to_string())
                        .or_default()
                        .insert(delta.clone(), diffstat.clone());
                }

                Ok::<_, FetchAndDiffError>(diffstat)
            })
            .await?;
        Ok(diffstat.clone())
    }

    /// Run a garbage-collection pass over the cache, removing any files which
    /// aren't supposed to be there, or which haven't been touched for an
    /// extended period of time.
    pub async fn gc(&self, max_package_age: Duration) {
        if self.root.is_none() {
            return;
        }

        let (root_rv, empty_rv, packages_rv) = tokio::join!(
            self.gc_root(),
            self.gc_empty(),
            self.gc_packages(max_package_age)
        );
        if let Err(err) = root_rv {
            error!("gc: performing gc on the cache root failed: {err}");
        }
        if let Err(err) = empty_rv {
            error!("gc: performing gc on the empty package failed: {err}");
        }
        if let Err(err) = packages_rv {
            error!("gc: performing gc on the package cache failed: {err}");
        }
    }

    /// Sync version of `gc`
    pub fn gc_sync(&self, max_package_age: Duration) {
        tokio::runtime::Handle::current().block_on(self.gc(max_package_age));
    }

    /// Remove any unrecognized files from the root of the cargo-vet cache
    /// directory.
    async fn gc_root(&self) -> Result<(), io::Error> {
        let root = self.root.as_ref().unwrap();
        let mut root_entries = tokio::fs::read_dir(root).await?;
        while let Some(entry) = root_entries.next_entry().await? {
            if !entry
                .file_name()
                .to_str()
                .map_or(false, |name| CACHE_ALLOWED_FILES.contains(&name))
            {
                remove_dir_entry(&entry).await?;
            }
        }
        Ok(())
    }

    /// Remove all files located in the `cargo-vet/empty` directory, as it
    /// should be empty.
    async fn gc_empty(&self) -> Result<(), std::io::Error> {
        let empty = self.root.as_ref().unwrap().join(CACHE_EMPTY_PACKAGE);
        let mut empty_entries = tokio::fs::read_dir(&empty).await?;
        while let Some(entry) = empty_entries.next_entry().await? {
            remove_dir_entry(&entry).await?;
        }
        Ok(())
    }

    /// Remove any non '.crate' files from the registry cache, '.crate' files
    /// which are older than `max_package_age`, and any source directories from
    /// the registry src which no longer have a corresponding .crate.
    async fn gc_packages(&self, max_package_age: Duration) -> Result<(), io::Error> {
        let cache = self.root.as_ref().unwrap().join(CACHE_REGISTRY_CACHE);
        let src = self.root.as_ref().unwrap().join(CACHE_REGISTRY_SRC);

        let mut kept_packages = Vec::new();

        let mut cache_entries = tokio::fs::read_dir(&cache).await?;
        while let Some(entry) = cache_entries.next_entry().await? {
            if let Some(to_keep) = should_keep_package(&entry, max_package_age).await {
                kept_packages.push(to_keep);
            } else {
                remove_dir_entry(&entry).await?;
            }
        }

        let mut src_entries = tokio::fs::read_dir(&src).await?;
        while let Some(entry) = src_entries.next_entry().await? {
            if !kept_packages.contains(&entry.file_name()) || !fetch_is_ok(&entry.path()).await {
                remove_dir_entry(&entry).await?;
            }
        }
        Ok(())
    }

    /// Delete every file in the cache directory other than the cache lock, and
    /// clear out the command history and diff cache files.
    ///
    /// NOTE: The diff_cache, command_history, and publisher_cache files will be
    /// re-created when the cache is unlocked, however they will be empty.
    pub async fn clean(&self) -> Result<(), io::Error> {
        let root = self.root.as_ref().expect("cannot clean a mocked cache");

        // Make sure we don't write back the command history, diff cache, or
        // publisher cache when dropping.
        {
            let mut guard = self.state.lock().unwrap();
            guard.command_history = Default::default();
            guard.diff_cache = Default::default();
            guard.publisher_cache = Default::default();
        }

        let mut root_entries = tokio::fs::read_dir(&root).await?;
        while let Some(entry) = root_entries.next_entry().await? {
            if entry.file_name() != Path::new(CACHE_VET_LOCK) {
                remove_dir_entry(&entry).await?;
            }
        }
        Ok(())
    }

    /// Sync version of `clean`
    pub fn clean_sync(&self) -> Result<(), io::Error> {
        tokio::runtime::Handle::current().block_on(self.clean())
    }

    pub fn get_last_fetch(&self) -> Option<FetchCommand> {
        let guard = self.state.lock().unwrap();
        guard.command_history.last_fetch.clone()
    }

    pub fn set_last_fetch(&self, last_fetch: FetchCommand) {
        let mut guard = self.state.lock().unwrap();
        guard.command_history.last_fetch = Some(last_fetch);
    }

    /// Look up information about who published each version of the specified
    /// crates. Versions for each crate are also specified in order to avoid
    /// hitting the network in the case where the cache already has the relevant
    /// information.
    fn get_publishers(
        &self,
        network: &Network,
        name: PackageStr<'_>,
        versions: FastSet<&semver::Version>,
    ) -> Result<Vec<PublisherCacheVersion>, FetchAuditError> {
        let now: chrono::DateTime<chrono::Utc> = std::time::SystemTime::now().into();

        // Load the cached response from our publisher-cache.
        {
            let guard = self.state.lock().unwrap();
            if let Some(published) = guard.publisher_cache.crates.get(name) {
                // Check if there are any relevant versions which are not present in
                // the local cache. If none are missing, we have everything cached
                // and can continue as normal.
                let missing_versions: Vec<_> = versions
                    .iter()
                    .filter(|&&v| !published.versions.iter().any(|p| &p.num == v))
                    .collect();
                if missing_versions.is_empty() {
                    info!("using cached publisher info for {name} - relevant versions in cache");
                    return Ok(published.versions.clone());
                }

                // If we last fetched this package's published versions less than a
                // day ago, double-check if the versions we care about appear in the
                // local crates.io index.
                // If none of the versions appear in the local index, we can skip
                // fetching. This should help in cases where a crate is marked as
                // `audit-as-crates-io` but is not actually published, as we'll
                // never find publisher information in those cases.
                if now - published.last_fetched
                    < chrono::Duration::days(NONINDEX_VERSION_PUBLISHER_REFRESH_DAYS)
                {
                    if let Some(index_crate) = self.query_package_from_index(name) {
                        if missing_versions
                            .iter()
                            .all(|version| exact_version(&index_crate, version).is_none())
                        {
                            info!("using cached publisher info for {name} - missing versions appear unpublished");
                            return Ok(published.versions.clone());
                        }
                    }
                }
            }
        }

        // If we don't know the publisher for every "relevant" version
        // of this crate, we want to make sure we have the most
        // up-to-date information about the publisher of packages from
        // crates.io, so need to fetch information from the crates.io
        // API.
        //
        // NOTE: The official scraper policy requests a rate limit of 1
        // request per second (https://crates.io/policies#crawlers).
        // This wouldn't be a very good user-experience to require a
        // multi-second wait to fetch each crate's information, however
        // the local caching and infrequent user-driven calls to the API
        // should hopefully ensure we remain under the 1 request per
        // second limit over time.
        //
        // If this ends up being an issue, we can look into adding some form
        // of cross-call tracking in the cache to ensure that we don't
        // exceed the rate over a slightly-extended period of time, (e.g. by
        // throttling requests from consecutive calls).
        assert!(!name.contains('/'), "invalid crate name");
        let url = Url::parse(&format!("https://crates.io/api/v1/crates/{name}"))
            .expect("invalid crate name");

        // NOTE: Our caller isn't able to do anything else at the same
        // time, and we don't want to do multiple crates.io API calls at
        // the same time, so we'll do the network fetch sync for now.
        let response = tokio::runtime::Handle::current().block_on(network.download(url))?;
        let result = load_json::<CratesAPICrate>(&response[..])?.versions;

        // Update the users cache and individual crates caches, and return our
        // set of versions.
        let mut guard = self.state.lock().unwrap();
        let versions: Vec<_> = result
            .into_iter()
            .map(|api_version| PublisherCacheVersion {
                num: api_version.num,
                created_at: api_version.created_at,
                published_by: api_version.published_by.map(|api_user| {
                    info!("recording user info for {api_user:?}");
                    guard.publisher_cache.users.insert(
                        api_user.id,
                        PublisherCacheUser {
                            login: api_user.login,
                            name: api_user.name,
                        },
                    );
                    api_user.id
                }),
            })
            .collect();
        info!("found {} versions for crate {}", versions.len(), name);
        guard.publisher_cache.crates.insert(
            name.to_owned(),
            PublisherCacheEntry {
                last_fetched: now,
                versions: versions.clone(),
            },
        );

        Ok(versions)
    }

    /// Look up user information for a crates.io user from the publisher cache.
    fn get_crates_user_info(&self, user_id: u64) -> Option<PublisherCacheUser> {
        let guard = self.state.lock().unwrap();
        guard.publisher_cache.users.get(&user_id).cloned()
    }
}

/// Queries a package in the crates.io registry for a specific published version
pub fn exact_version<'a>(
    this: &'a crates_index::Crate,
    target_version: &semver::Version,
) -> Option<&'a crates_index::Version> {
    for index_version in this.versions() {
        if let Ok(index_ver) = index_version.version().parse::<semver::Version>() {
            if &index_ver == target_version {
                return Some(index_version);
            }
        }
    }
    None
}

/// Locate the checkout path for the given package and version if it is part of
/// the local build graph. Returns `None` if a local checkout cannot be found.
pub fn locate_local_checkout(
    metadata: &cargo_metadata::Metadata,
    package: PackageStr<'_>,
    version: &VetVersion,
) -> Option<PathBuf> {
    for pkg in &metadata.packages {
        if pkg.name == package && &pkg.vet_version() == version {
            assert_eq!(
                pkg.manifest_path.file_name(),
                Some(CARGO_TOML_FILE),
                "unexpected manifest file name"
            );
            return Some(pkg.manifest_path.parent().map(PathBuf::from).unwrap());
        }
    }
    None
}

#[tracing::instrument(err)]
fn unpack_package(tarball: &File, unpack_dir: &Path) -> Result<(), UnpackError> {
    // If we get here and the unpack_dir exists, this implies we had a previously failed fetch,
    // blast it away so we can have a clean slate!
    if unpack_dir.exists() {
        fs::remove_dir_all(unpack_dir)?;
    }
    fs::create_dir(unpack_dir)?;
    let gz = GzDecoder::new(tarball);
    let mut tar = Archive::new(gz);
    let prefix = unpack_dir.file_name().unwrap();
    let parent = unpack_dir.parent().unwrap();
    for entry in tar.entries()? {
        let mut entry = entry.map_err(UnpackError::ArchiveIterate)?;
        let entry_path = entry
            .path()
            .map_err(UnpackError::ArchiveEntry)?
            .into_owned();

        // We're going to unpack this tarball into the global source
        // directory, but we want to make sure that it doesn't accidentally
        // (or maliciously) overwrite source code from other crates. Cargo
        // itself should never generate a tarball that hits this error, and
        // crates.io should also block uploads with these sorts of tarballs,
        // but be extra sure by adding a check here as well.
        if !entry_path.starts_with(prefix) {
            return Err(UnpackError::InvalidPaths {
                entry_path,
                prefix: prefix.to_owned(),
            });
        }

        entry
            .unpack_in(parent)
            .map_err(|error| UnpackError::Unpack {
                entry_path: entry_path.clone(),
                error,
            })?;
    }

    create_unpack_lock(unpack_dir).map_err(|error| UnpackError::LockCreate {
        target: unpack_dir.to_owned(),
        error,
    })?;

    Ok(())
}

fn create_unpack_lock(unpack_dir: &Path) -> Result<(), io::Error> {
    let lockfile = unpack_dir.join(CARGO_OK_FILE);

    // The lock file is created after unpacking so we overwrite a lock file
    // which may have been extracted from the package.
    let mut ok = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lockfile)?;

    // Write to the lock file to indicate that unpacking was successful.
    write!(ok, "ok")?;
    ok.sync_all()?;

    Ok(())
}

/// Unpack a non-crates.io package checkout in a format similar to what would be
/// unpacked from a .crate file published on crates.io.
///
/// This is used in order to normalize the file and directory structure for git
/// revisions to make them easier to work with when diffing.
async fn unpack_checkout(
    checkout_path: &Path,
    unpack_path: &Path,
) -> Result<(), UnpackCheckoutError> {
    // Invoke `cargo package --list` to determine the list of files which
    // should be copied to the repackaged directory.
    let cargo_path = std::env::var_os(CARGO_ENV).expect("Cargo failed to set $CARGO, how?");
    let out = tokio::process::Command::new(cargo_path)
        .arg("package")
        .arg("--list")
        .arg("--allow-dirty")
        .arg("--manifest-path")
        .arg(checkout_path.join(CARGO_TOML_FILE))
        .output()
        .await
        .map_err(CommandError::CommandFailed)?;

    if !out.status.success() {
        return Err(CommandError::BadStatus(out.status.code().unwrap_or(-1)).into());
    }

    let stdout = String::from_utf8(out.stdout).map_err(CommandError::BadOutput)?;

    tokio::fs::create_dir_all(unpack_path)
        .await
        .map_err(|error| UnpackCheckoutError::CreateDirError {
            path: unpack_path.to_owned(),
            error,
        })?;

    // Asynchronously copy all required files to the target directory.
    try_join_all(stdout.lines().map(|target| async move {
        // We'll be ignoring diffs for each of the skipped paths, so we can
        // ignore these if cargo reports them.
        if DIFF_SKIP_PATHS.iter().any(|&p| p == target) {
            return Ok(());
        }

        let to = unpack_path.join(target);
        let from = match target {
            // Copy the original Cargo.toml to Cargo.toml.orig for better
            // comparisons.
            "Cargo.toml.orig" => checkout_path.join(CARGO_TOML_FILE),
            _ => checkout_path.join(target),
        };

        // Create the directory this file will be placed in.
        let parent = to.parent().unwrap();
        tokio::fs::create_dir_all(&parent).await.map_err(|error| {
            UnpackCheckoutError::CreateDirError {
                path: parent.to_owned(),
                error,
            }
        })?;

        match tokio::fs::copy(from, to).await {
            Ok(_) => Ok(()),
            Err(error) => match error.kind() {
                // Cargo may tell us about files which don't exist (e.g. because
                // they are generated). It's OK to ignore those files when
                // copying.
                io::ErrorKind::NotFound => Ok(()),
                _ => Err(UnpackCheckoutError::CopyError {
                    target: target.into(),
                    error,
                }),
            },
        }
    }))
    .await?;

    let unpack_path_ = unpack_path.to_owned();
    tokio::task::spawn_blocking(move || create_unpack_lock(&unpack_path_))
        .await
        .expect("failed to join")
        .map_err(UnpackCheckoutError::LockCreate)?;

    Ok(())
}

async fn fetch_is_ok(fetch: &Path) -> bool {
    match tokio::fs::read_to_string(fetch.join(CARGO_OK_FILE)).await {
        Ok(ok) => ok == CARGO_OK_BODY,
        Err(_) => false,
    }
}

/// Based on the type of file for an entry, either recursively remove the
/// directory, or remove the file. This is intended to be roughly equivalent to
/// `rm -r`.
async fn remove_dir_entry(entry: &tokio::fs::DirEntry) -> Result<(), io::Error> {
    info!("gc: removing {}", entry.path().display());
    let file_type = entry.file_type().await?;
    if file_type.is_dir() {
        tokio::fs::remove_dir_all(entry.path()).await?;
    } else {
        tokio::fs::remove_file(entry.path()).await?;
    }
    Ok(())
}

/// Given a directory entry for a file, returns how old it is. If there is an
/// issue (e.g. mtime >= now), will return `None` instead.
async fn get_file_age(entry: &tokio::fs::DirEntry) -> Option<Duration> {
    let now = SystemTime::now();
    let meta = entry.metadata().await.ok()?;
    now.duration_since(meta.modified().ok()?).ok()
}

/// Returns tne name of the crate if it should be preserved, or `None` if it shouldn't.
async fn should_keep_package(
    entry: &tokio::fs::DirEntry,
    max_package_age: Duration,
) -> Option<OsString> {
    // Get the stem and extension from the directory entry's path, and
    // immediately remove it if something goes wrong.
    let path = entry.path();
    let stem = path.file_stem()?;
    if path.extension()? != OsStr::new("crate") {
        return None;
    }

    match get_file_age(entry).await {
        Some(age) if age > max_package_age => None,
        _ => Some(stem.to_owned()),
    }
}

fn find_cargo_registry() -> Result<CargoRegistry, crates_index::Error> {
    // ERRORS: all of this is genuinely fallible internal workings
    // but if these path adjustments don't work then something is very fundamentally wrong

    let index = CratesIndex::new_cargo_default()?;

    let base_dir = index.path().parent().unwrap().parent().unwrap().to_owned();
    let registry = index.path().file_name().unwrap().to_owned();

    Ok(CargoRegistry {
        index,
        base_dir,
        registry,
        index_up_to_date: false,
    })
}

fn load_toml<T>(file_name: &str, reader: impl Read) -> Result<(SourceFile, T), LoadTomlError>
where
    T: for<'a> Deserialize<'a>,
{
    let mut reader = BufReader::new(reader);
    let mut string = String::new();
    reader.read_to_string(&mut string)?;
    let source_code = SourceFile::new(file_name, string);
    let result = toml::de::from_str(source_code.source());
    match result {
        Ok(toml) => Ok((source_code, toml)),
        Err(error) => {
            let (line, col) = error.line_col().unwrap_or((0, 0));
            let span = SourceOffset::from_location(source_code.source(), line + 1, col);
            Err(TomlParseError {
                source_code,
                span,
                error,
            }
            .into())
        }
    }
}
fn store_toml<T>(heading: &str, val: T) -> Result<String, StoreTomlError>
where
    T: Serialize,
{
    let toml_document = to_formatted_toml(val)?;
    Ok(format!("{heading}{toml_document}"))
}
fn load_json<T>(reader: impl Read) -> Result<T, LoadJsonError>
where
    T: for<'a> Deserialize<'a>,
{
    let mut reader = BufReader::new(reader);
    let mut string = String::new();
    reader.read_to_string(&mut string)?;
    let json = serde_json::from_str(&string).map_err(|error| JsonParseError { error })?;
    Ok(json)
}
fn store_json<T>(val: T) -> Result<String, StoreJsonError>
where
    T: Serialize,
{
    let json_string = serde_json::to_string(&val)?;
    Ok(json_string)
}

fn store_audits(mut audits: AuditsFile) -> Result<String, StoreTomlError> {
    let heading = r###"
# cargo-vet audits file
"###;
    audits
        .audits
        .values_mut()
        .for_each(|entries| entries.sort());

    store_toml(heading, audits)
}
fn store_config(mut config: ConfigFile) -> Result<String, StoreTomlError> {
    config
        .exemptions
        .values_mut()
        .for_each(|entries| entries.sort());

    let heading = r###"
# cargo-vet config file
"###;

    store_toml(heading, config)
}
fn store_imports(imports: ImportsFile) -> Result<String, StoreTomlError> {
    let heading = r###"
# cargo-vet imports lock
"###;

    store_toml(heading, imports)
}
fn store_diff_cache(diff_cache: DiffCache) -> Result<String, StoreTomlError> {
    let heading = "";

    store_toml(heading, diff_cache)
}
fn store_command_history(command_history: CommandHistory) -> Result<String, StoreJsonError> {
    store_json(command_history)
}
fn store_publisher_cache(publisher_cache: PublisherCache) -> Result<String, StoreJsonError> {
    store_json(publisher_cache)
}
