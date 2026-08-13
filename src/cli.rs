use crate::{
    backup,
    paths::{AppPaths, DEFAULT_PORT, validate_port},
    server,
    store::{self, Store},
};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

/// Test hook that makes `check` report a blocking pre-flight failure, so the
/// "installation verification fails before any switch" drill is deterministic.
const TEST_FAIL_CHECK_VARIABLE: &str = "LOCAL_API_RELAY_TEST_FAIL_CHECK";

/// The version this process reports. The compiled-in crate version is the real
/// value; the test hook overrides it for upgrade drills.
fn displayed_version() -> &'static str {
    if let Ok(value) = std::env::var(crate::log::TEST_VERSION_VARIABLE)
        && !value.is_empty()
    {
        return Box::leak(value.into_boxed_str());
    }
    env!("CARGO_PKG_VERSION")
}

#[derive(Debug, Parser)]
#[command(
    name = "local-api-relay",
    version = displayed_version(),
    about = "Loopback-only local API relay"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate the one-time bootstrap credential for the local administrator.
    InitAdmin,
    /// Start the loopback-only relay service.
    Serve {
        /// Override the configured stable loopback port for this process.
        #[arg(long, value_parser = parse_port)]
        port: Option<u16>,
    },
    /// Verify an upgrade's preconditions before the stable entry is switched
    /// (PKG-013): process configuration compatibility, the live database
    /// schema, and whether a forward migration is required. Read-only — it
    /// never migrates or writes the database. Exits non-zero on a blocking
    /// condition (newer schema, corrupt database, invalid settings).
    Check,
    /// Create a verified managed backup of the live database without running
    /// any migration: the upgrade pre-flight's pre-migration snapshot, which
    /// must exist and verify before the stable entry is switched and which the
    /// rollback restores to undo a committed forward migration (PKG-014).
    Backup {
        /// The trigger recorded on the artifact.
        #[arg(long, value_parser = parse_trigger, default_value = "migration")]
        reason: backup::TriggerKind,
    },
    /// Explicitly restore a managed backup into the live database at the file
    /// level. Unlike the management API this works when the live database is
    /// newer than this binary supports, which is exactly the state a rollback
    /// to a previous version must repair (DATA-014/015/016).
    Restore {
        /// The managed backup artifact name to restore.
        name: String,
    },
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port = value
        .parse::<u16>()
        .map_err(|_| "port must be a number".to_owned())?;
    validate_port(port).map_err(|error| error.to_string())?;
    Ok(port)
}

fn parse_trigger(value: &str) -> Result<backup::TriggerKind, String> {
    match value {
        "auto" => Ok(backup::TriggerKind::Auto),
        "manual" => Ok(backup::TriggerKind::Manual),
        "migration" => Ok(backup::TriggerKind::Migration),
        "restore" => Ok(backup::TriggerKind::Restore),
        _ => Err("trigger must be one of auto, manual, migration, restore".to_owned()),
    }
}

fn fail_check_if_requested() -> Result<()> {
    if std::env::var(TEST_FAIL_CHECK_VARIABLE).ok().as_deref() == Some("1") {
        bail!("injected preflight check failure");
    }
    Ok(())
}

pub async fn run(cli: Cli) -> Result<()> {
    let paths = AppPaths::discover()?;
    paths.prepare()?;
    let settings = paths.load_or_create_settings()?;

    match cli.command {
        Command::InitAdmin => {
            let mut store = Store::open(paths.database_path(), paths.backup_dir())?;
            let credential = store.initialize_administrator()?;
            println!("Administrator bootstrap credential: {credential}");
            Ok(())
        }
        Command::Serve { port } => {
            let port = port.unwrap_or(settings.port);
            validate_port(port)?;
            server::serve(
                paths.database_path(),
                paths.backup_dir(),
                paths.state_dir.join("logs"),
                port,
            )
            .await
        }
        Command::Check => {
            let supported = store::supported_schema();
            let database_path = paths.database_path();
            let database_schema = store::read_live_schema(&database_path)?;
            if database_path.exists() && database_schema.is_none() {
                bail!(
                    "SQLite database at {} exists but has no local-api-relay schema; \
                     it was left untouched",
                    database_path.display()
                );
            }
            let migration_needed = database_schema.is_some_and(|schema| schema < supported);
            println!("version={}", displayed_version());
            println!("supported_schema={supported}");
            println!("port={}", settings.port);
            match database_schema {
                Some(schema) => println!("database_schema={schema}"),
                None => println!("database_schema=none"),
            }
            println!("migration_needed={migration_needed}");
            if let Some(schema) = database_schema
                && schema > supported
            {
                bail!(
                    "database schema {schema} is newer than this binary supports ({supported}); \
                     the upgrade is blocked and the database was left untouched"
                );
            }
            fail_check_if_requested()?;
            Ok(())
        }
        Command::Backup { reason } => {
            let artifact =
                store::create_backup_at_paths(&paths.database_path(), &paths.backup_dir(), reason)?;
            println!("backup={}", artifact.name);
            Ok(())
        }
        Command::Restore { name } => {
            let outcome =
                store::restore_database_at_paths(&paths.database_path(), &paths.backup_dir(), &name)?;
            println!("restored_from={}", outcome.candidate_name);
            println!("restored_schema={}", outcome.restored_schema);
            println!("pre_restore_backup={}", outcome.pre_restore_backup_name);
            Ok(())
        }
    }
}

#[allow(dead_code)]
const _: u16 = DEFAULT_PORT;
