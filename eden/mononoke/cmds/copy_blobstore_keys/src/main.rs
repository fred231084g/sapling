/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Tool to copy a list of blobs from one repo to another.
//! The tool is quite dumb - it just copies the blobs in any order so e.g. aliases might be copied
//! after the file content was copied. So it should be used only in the urgent situations
//! like e.g. repo corruption. List of keys are passed in a file, and this list of keys
//! can be generated by any tool e.g. walker.
//! It's similar to manual_scrub tool, with the exception that manual_scrub preserves the repoid
//! prefix for the blob, while this tool either strips it or ignores it.

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Error;
use blobrepo::BlobRepo;
use blobstore::Blobstore;
use blobstore::PutBehaviour;
use clap::Arg;
use cmdlib::args::get_config_by_repoid;
use cmdlib::args::load_common_config;
use cmdlib::args::MononokeMatches;
use cmdlib::args::{self};
use cmdlib::helpers::setup_repo_dir;
use cmdlib::helpers::CreateStorage;
use context::CoreContext;
use context::SessionClass;
use fbinit::FacebookInit;
use futures::future;
use futures::stream;
use futures::StreamExt;
use futures::TryStreamExt;
use metaconfig_types::BlobConfig;
use metaconfig_types::BlobstoreId;
use mononoke_types::RepositoryId;
use repo_factory::RepoFactory;
use slog::debug;
use slog::info;
use slog::warn;
use slog::Logger;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio_stream::wrappers::LinesStream;

const ARG_CONCURRENCY: &str = "concurrency";
const ARG_ERROR_KEYS: &str = "error-keys-output";
const ARG_IGNORE_ERRORS: &str = "ignore-errors";
const ARG_IN_FILE: &str = "input-file";
const ARG_MISSING_KEYS: &str = "missing-keys-output";
const ARG_SUCCESSFUL_KEYS: &str = "success-keys-output";
const ARG_STRIP_SOURCE_REPO_PREFIX: &str = "strip-source-repo-prefix";
const ARG_TARGET_INNER_BLOBSTORE_ID: &str = "target-inner-blobstore-id";
const ARG_SOURCE_INNER_BLOBSTORE_ID: &str = "source-inner-blobstore-id";

struct OutputFiles {
    error_file: File,
    missing_file: File,
    successful_file: File,
}

impl OutputFiles {
    pub async fn new(matches: &MononokeMatches<'_>) -> Result<Self, Error> {
        let error_file = Self::open_file(matches, ARG_ERROR_KEYS);
        let missing_file = Self::open_file(matches, ARG_MISSING_KEYS);
        let successful_file = Self::open_file(matches, ARG_SUCCESSFUL_KEYS);

        let (error_file, missing_file, successful_file) =
            future::try_join3(error_file, missing_file, successful_file).await?;

        Ok(Self {
            error_file,
            missing_file,
            successful_file,
        })
    }

    pub async fn record_copy_result(
        &mut self,
        key: &str,
        res: Result<(), CopyError>,
    ) -> Result<(), Error> {
        let file = match res {
            Ok(()) => &mut self.successful_file,
            Err(CopyError::NotFound) => &mut self.missing_file,
            Err(CopyError::Error(_)) => &mut self.error_file,
        };

        file.write_all(key.as_bytes()).await?;
        file.write(b"\n").await?;

        res.with_context(|| format!("failed to copy {}", key))?;

        Ok(())
    }

    async fn open_file(matches: &MononokeMatches<'_>, arg: &str) -> Result<File, Error> {
        let filename = matches
            .value_of(arg)
            .ok_or_else(|| anyhow!("{} is not set", arg))?;
        let file = File::create(filename).await?;

        Ok(file)
    }
}

#[derive(Error, Debug)]
enum CopyError {
    #[error("Not found")]
    NotFound,
    #[error(transparent)]
    Error(#[from] Error),
}

async fn run<'a>(fb: FacebookInit, matches: &'a MononokeMatches<'a>) -> Result<(), Error> {
    let logger = matches.logger();
    let mut ctx = CoreContext::new_with_logger(fb, logger.clone());
    // Background session class tells multiplexed blobstore to wait
    // for all blobstores to finish.
    // TODO(stash): T89677672 at the moment Background session class writes to
    // blobstore sync queue if blobstore has failed. This might not be what we
    // want here.
    ctx.session_mut()
        .override_session_class(SessionClass::Background);

    let source_repo_id = args::get_source_repo_id(matches.config_store(), matches)?;
    let source_inner_blobstore_id = args::get_u64_opt(matches, ARG_SOURCE_INNER_BLOBSTORE_ID);

    let source_repo = open_repo(
        ctx.logger(),
        source_repo_id,
        source_inner_blobstore_id,
        &matches,
    );

    let target_repo_id = args::get_target_repo_id(matches.config_store(), matches)?;
    let target_inner_blobstore_id = args::get_u64_opt(matches, ARG_TARGET_INNER_BLOBSTORE_ID);

    let target_repo = open_repo(
        ctx.logger(),
        target_repo_id,
        target_inner_blobstore_id,
        &matches,
    );

    let (source_repo, target_repo) = future::try_join(source_repo, target_repo).await?;

    let mut keys = vec![];
    let source_repo_prefix = source_repo_id.prefix();

    let strip_source_repo_prefix = matches.is_present(ARG_STRIP_SOURCE_REPO_PREFIX);
    let inputfile = matches
        .value_of(ARG_IN_FILE)
        .ok_or_else(|| anyhow!("{} not set", ARG_IN_FILE))?;
    let mut inputfile = File::open(inputfile).await?;
    let file = BufReader::new(&mut inputfile);
    let mut lines = LinesStream::new(file.lines());
    while let Some(line) = lines.try_next().await? {
        if strip_source_repo_prefix {
            match line.strip_prefix(&source_repo_prefix) {
                Some(key) => {
                    keys.push(key.to_string());
                }
                None => {
                    return Err(anyhow!(
                        "key {} doesn't start with prefix {}",
                        line,
                        source_repo_prefix
                    ));
                }
            }
        } else {
            keys.push(line);
        }
    }

    let concurrency = args::get_usize(matches, ARG_CONCURRENCY, 100);
    let ignore_errors = matches.is_present(ARG_IGNORE_ERRORS);

    info!(ctx.logger(), "{} keys to copy", keys.len());
    let log_step = std::cmp::max(1, keys.len() / 10);

    let mut s = stream::iter(keys)
        .map(|key| async {
            let copy_key = key.clone();
            let res = async {
                let source_blobstore = source_repo.get_blobstore();
                let target_blobstore = target_repo.get_blobstore();
                let maybe_value = source_blobstore.get(&ctx, &key).await?;
                let value = maybe_value.ok_or(CopyError::NotFound)?;
                debug!(ctx.logger(), "copying {}", key);
                target_blobstore.put(&ctx, key, value.into_bytes()).await?;
                Result::<_, CopyError>::Ok(())
            }
            .await;

            (copy_key, res)
        })
        .buffered(concurrency);

    let mut copied = 0;
    let mut processed = 0;
    let mut output_files = OutputFiles::new(matches).await?;
    while let Some((key, res)) = s.next().await {
        let res = output_files.record_copy_result(&key, res).await;
        match res {
            Ok(()) => {
                copied += 1;
            }
            Err(err) => {
                if ignore_errors {
                    warn!(ctx.logger(), "key: {} {:#}", key, err);
                } else {
                    return Err(err);
                }
            }
        };
        processed += 1;
        if processed % log_step == 0 {
            info!(ctx.logger(), "{} keys processed", processed);
        }
    }

    info!(ctx.logger(), "{} keys were copied", copied);
    Ok(())
}

async fn open_repo<'a>(
    logger: &Logger,
    repo_id: RepositoryId,
    maybe_inner_blobstore_id: Option<u64>,
    matches: &'a MononokeMatches<'a>,
) -> Result<BlobRepo, Error> {
    let config_store = matches.config_store();
    let common_config = load_common_config(config_store, &matches)?;
    let (reponame, mut config) = get_config_by_repoid(config_store, matches, repo_id)?;
    if let Some(inner_id) = maybe_inner_blobstore_id {
        override_blobconfig(&mut config.storage_config.blobstore, inner_id)?;
    }
    info!(logger, "using repo \"{}\" repoid {:?}", reponame, repo_id);
    match &config.storage_config.blobstore {
        BlobConfig::Files { path } | BlobConfig::Sqlite { path } => {
            setup_repo_dir(path, CreateStorage::ExistingOnly)?;
        }
        _ => {}
    };

    let repo_factory = RepoFactory::new(matches.environment().clone(), &common_config);

    let repo = repo_factory.build(reponame, config).await?;

    Ok(repo)
}

fn override_blobconfig(blob_config: &mut BlobConfig, inner_blobstore_id: u64) -> Result<(), Error> {
    match blob_config {
        BlobConfig::Multiplexed { ref blobstores, .. } => {
            let sought_id = BlobstoreId::new(inner_blobstore_id);
            let inner_blob_config = blobstores
                .iter()
                .find_map(|(blobstore_id, _, blobstore)| {
                    if blobstore_id == &sought_id {
                        Some(blobstore.clone())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    anyhow!("could not find a blobstore with id {}", inner_blobstore_id)
                })?;
            *blob_config = inner_blob_config;
        }
        _ => {
            return Err(anyhow!(
                "inner-blobstore-id supplied but blobstore is not multiplexed"
            ));
        }
    }
    Ok(())
}

#[fbinit::main]
fn main(fb: FacebookInit) -> Result<(), Error> {
    let matches = args::MononokeAppBuilder::new(
        "Tool to copy a list of blobs from one blobstore to another",
    )
    .with_advanced_args_hidden()
    .with_source_and_target_repos()
    .with_special_put_behaviour(PutBehaviour::Overwrite)
    .build()
    .arg(
        Arg::with_name(ARG_IN_FILE)
            .long(ARG_IN_FILE)
            .required(true)
            .takes_value(true)
            .help("input filename with a list of keys"),
    )
    .arg(
        Arg::with_name(ARG_CONCURRENCY)
            .long(ARG_CONCURRENCY)
            .required(false)
            .takes_value(true)
            .help("How many blobs to copy at once"),
    )
    .arg(
        Arg::with_name(ARG_IGNORE_ERRORS)
            .long(ARG_IGNORE_ERRORS)
            .required(false)
            .takes_value(false)
            .help("Don't terminate if failed to process a key"),
    )
    .arg(
        Arg::with_name(ARG_STRIP_SOURCE_REPO_PREFIX)
            .long(ARG_STRIP_SOURCE_REPO_PREFIX)
            .required(false)
            .takes_value(false)
            .help(
                "If a key starts with 'repoXXXX' prefix \
                      (where XXXX matches source repository) then strip this \
                      prefix before copying",
            ),
    )
    .arg(
        Arg::with_name(ARG_SUCCESSFUL_KEYS)
            .long(ARG_SUCCESSFUL_KEYS)
            .takes_value(true)
            .required(true)
            .help("A file to write successfully copied keys to"),
    )
    .arg(
        Arg::with_name(ARG_MISSING_KEYS)
            .long(ARG_MISSING_KEYS)
            .takes_value(true)
            .required(true)
            .help("A file to write missing keys to"),
    )
    .arg(
        Arg::with_name(ARG_ERROR_KEYS)
            .long(ARG_ERROR_KEYS)
            .takes_value(true)
            .required(true)
            .help("A file to write error fetching keys to"),
    )
    .arg(
        Arg::with_name(ARG_TARGET_INNER_BLOBSTORE_ID)
            .long(ARG_TARGET_INNER_BLOBSTORE_ID)
            .takes_value(true)
            .required(false)
            .requires(ARG_SOURCE_INNER_BLOBSTORE_ID)
            .help("In case of multiplexed blobstore this will be target id of inner blobstore"),
    )
    .arg(
        Arg::with_name(ARG_SOURCE_INNER_BLOBSTORE_ID)
            .long(ARG_SOURCE_INNER_BLOBSTORE_ID)
            .takes_value(true)
            .required(false)
            .requires(ARG_TARGET_INNER_BLOBSTORE_ID)
            .help("In case of multiplexed blobstore this will be source id of inner blobstore"),
    )
    .get_matches(fb)?;

    matches.runtime().block_on(run(fb, &matches))
}
