mod cli;
mod config;
mod git;
mod keepassxc;
mod utils;

use anyhow::{anyhow, Result};
use clap::Parser;
use cli::{EntryFilters, GetMode, HasEntryFilters, UnlockOptions};
use config::{Caller, Config, Database};
use crypto_box::{PublicKey, SecretKey};
use git::GitCredentialMessage;
use keepassxc::{errors::*, messages::*, Group};
use once_cell::sync::OnceCell;
use slog::{Drain, Level, Logger};
use std::env;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use tabwriter::TabWriter;
use utils::callers::CurrentCaller;
use utils::*;

use crate::cli::HasLocalEntryFilters;

static LOGGER: OnceCell<Logger> = OnceCell::new();

fn exchange_keys<T: AsRef<str>>(client_id: T, session_pubkey: &PublicKey) -> Result<PublicKey> {
    // exchange public keys
    let cpr_req = ChangePublicKeysRequest::new(client_id.as_ref(), session_pubkey);
    let cpr_resp = cpr_req.send()?;
    cpr_resp
        .get_public_key()
        .ok_or_else(|| anyhow!("Failed to retrieve host public key"))
}

fn start_session() -> Result<(String, SecretKey, PublicKey)> {
    // generate keys for encrypting current session
    let session_seckey = generate_secret_key();
    let session_pubkey = session_seckey.public_key();

    // temporary client id
    let (_, client_id) = nacl_nonce();

    // exchange public keys
    let host_pubkey = exchange_keys(&client_id, &session_pubkey)?;

    // initialise crypto_box
    let _ = get_client_box(Some(&host_pubkey), Some(&session_seckey));

    Ok((client_id, session_seckey, host_pubkey))
}

fn associated_databases<T: AsRef<str>>(
    config: &Config,
    client_id: T,
    unlock_options: &Option<UnlockOptions>,
) -> Result<Vec<Database>> {
    let databases: Vec<_> = config
        .get_databases()?
        .iter()
        .filter(|db| {
            let mut remain_retries = unlock_options.as_ref().map_or_else(|| 0, |v| v.max_retries);
            let mut success = false;
            loop {
                let taso_req = TestAssociateRequest::new(db.id.as_str(), db.pkey.as_str());
                // trigger unlock if command line argument is given
                let taso_resp = taso_req.send(client_id.as_ref(), unlock_options.is_some());
                let database_locked = match &taso_resp {
                    Ok(_) => false,
                    Err(e) => {
                        if let Some(keepass_error) = e.downcast_ref::<KeePassError>() {
                            keepass_error.is_database_locked()
                        } else {
                            false
                        }
                    }
                };
                if let Ok((ref taso_resp, _)) = taso_resp {
                    success = taso_resp.success.as_ref().map_or(false, |s| *s.as_ref());
                }
                if taso_resp.is_err() || !success {
                    warn!(
                        "Failed to authenticate against database {} using stored key",
                        db.id
                    );
                }
                if success || !database_locked || unlock_options.is_none() {
                    break;
                }
                // loop get-databasehash until unlocked
                while remain_retries > 0 || unlock_options.as_ref().unwrap().max_retries == 0 {
                    warn!(
                        "Database {} is locked, gonna retry in {}ms (Remaining: {})",
                        db.id,
                        unlock_options.as_ref().unwrap().interval,
                        remain_retries
                    );
                    thread::sleep(Duration::from_millis(
                        unlock_options.as_ref().unwrap().interval,
                    ));

                    let gh_req = GetDatabaseHashRequest::new();
                    if gh_req.send(client_id.as_ref(), false).is_ok() {
                        info!("Database {} is unlocked", db.id);
                        break;
                    }
                    if unlock_options.as_ref().unwrap().max_retries != 0 {
                        remain_retries -= 1;
                    }
                }
                // still not unlocked, break
                if remain_retries == 0 && unlock_options.as_ref().unwrap().max_retries != 0 {
                    break;
                }
            }
            success
        })
        .cloned()
        .collect();
    if databases.is_empty() {
        Err(anyhow!(
            "No valid database associations found in configuration file"
        ))
    } else {
        info!(
            "Successfully authenticated against {} database(s)",
            databases.len()
        );
        Ok(databases)
    }
}

fn prompt_for_confirmation() -> Result<()> {
    print!("Press Enter to continue... ");
    std::io::stdout().flush()?;
    std::io::stdin().read_line(&mut String::new())?;
    Ok(())
}

fn handle_secondary_encryption(config_file: &mut Config) -> Result<()> {
    println!("There are existing encryption profile(s). If you'd like to reuse an existing encryption key, plug in the corresponding (hardware) token.");
    prompt_for_confirmation()?;
    if config_file.get_encryption_key().is_err() {
        warn!("Failed to extract encryption key from existing profiles");
        println!("Failed to extract the encryption key! Continue to configure a new (hardware) token using a DIFFERENT encryption key.")
    }
    println!("Now make sure you've plugged in the (hardware) token you'd like to use.");
    prompt_for_confirmation()?;
    Ok(())
}

fn configure<T: AsRef<Path>>(config_path: T, args: &cli::SubConfigureArgs) -> Result<()> {
    // read existing or create new config
    let mut config_file = if let Ok(config_file) = Config::read_from(&config_path) {
        verify_caller(&config_file)?;
        config_file
    } else {
        Config::new()
    };

    if config_file.count_callers() == 0 && cfg!(feature = "strict-caller") {
        warn!("Configuring database when strict-caller feature is enabled and no caller profiles are defined");
        println!("You are about to configure a new database before configuring any callers while strict-caller feature is enabled.");
        println!("You won't be able to use this program unless you plan to add caller profiles manually!");
        println!(
            "Tip: Check out `{} caller me --help` to add yourself to the allowed callers list.",
            env!("CARGO_BIN_NAME")
        );
        prompt_for_confirmation()?;
    }

    // start session
    let (client_id, session_seckey, _) = start_session()?;
    let session_pubkey = session_seckey.public_key();

    // generate permanent client key for future authentication
    let id_seckey = generate_secret_key();
    let id_pubkey = id_seckey.public_key();

    let aso_req = AssociateRequest::new(&session_pubkey, &id_pubkey);
    let (aso_resp, _) = aso_req.send(&client_id, false)?;
    let database_id = aso_resp.id.ok_or_else(|| anyhow!("Association failed"))?;

    // try to create a new group even if it already exists, KeePassXC will do the deduplication
    if args.group.is_empty() {
        return Err(anyhow!("Group name must not be empty"));
    }
    let cng_req = CreateNewGroupRequest::new(&args.group);
    let (cng_resp, _) = cng_req.send(&client_id, false)?;
    let group = Group::new(cng_resp.name, cng_resp.uuid);

    if let Some(ref encryption) = args.encrypt {
        if config_file.count_encryptions() > 0 && !encryption.is_empty() {
            handle_secondary_encryption(&mut config_file)?;
        }
        // this will error if an existing encryption profile has already been configured for the
        // underlying hardware/etc
        // in this case user should decrypt the configuration first
        config_file.add_encryption(encryption)?;
    }

    // save new config
    info!(
        "Saving configuration to {}",
        config_path.as_ref().to_string_lossy()
    );
    config_file.add_database(
        Database::new(database_id, id_seckey, group),
        args.encrypt.is_some(),
    )?;
    config_file.write_to(&config_path)?;

    Ok(())
}

fn encrypt<T: AsRef<Path>>(config_path: T, args: &cli::SubEncryptArgs) -> Result<()> {
    let mut config_file = Config::read_from(&config_path)?;
    verify_caller(&config_file)?;

    let count_databases_to_encrypt =
        config_file.count_databases() - config_file.count_encrypted_databases();
    let count_callers_to_encrypt =
        config_file.count_callers() - config_file.count_encrypted_callers();
    if count_databases_to_encrypt == 0
        && count_callers_to_encrypt == 0
        && args
            .encryption_profile
            .as_ref()
            .map(|m| m.is_empty())
            .unwrap_or_else(|| true)
    {
        warn!("Database and callers profiles have already been encrypted");
        return Ok(());
    }
    info!(
        "{} database profile(s) to encrypt",
        count_databases_to_encrypt
    );
    info!(
        "{} caller profile(s) to encrypt",
        count_databases_to_encrypt
    );

    if let Some(ref encryption) = args.encryption_profile {
        if config_file.count_encryptions() > 0 && !encryption.is_empty() {
            handle_secondary_encryption(&mut config_file)?;
        }
        // this will error if an existing encryption profile has already been configured for the
        // underlying hardware/etc
        // in this case user should decrypt the configuration first
        config_file.add_encryption(encryption)?;
    }

    let count_databases_encrypted = config_file.encrypt_databases()?;
    let count_callers_encrypted = config_file.encrypt_callers()?;
    info!(
        "{} database profile(s) encrypted",
        count_databases_encrypted
    );
    info!("{} caller profile(s) encrypted", count_callers_encrypted);

    config_file.write_to(config_path)?;

    Ok(())
}

fn decrypt<T: AsRef<Path>>(config_path: T) -> Result<()> {
    let mut config_file = Config::read_from(&config_path)?;
    verify_caller(&config_file)?;

    let count_databases_to_decrypt = config_file.count_encrypted_databases();
    let count_callers_to_decrypt = config_file.count_encrypted_callers();
    if count_databases_to_decrypt == 0 && count_callers_to_decrypt == 0 {
        warn!("Database and callers profiles have already been decrypted");
        return Ok(());
    }
    info!(
        "{} database profile(s) to decrypt",
        count_databases_to_decrypt
    );
    info!("{} caller profile(s) to decrypt", count_callers_to_decrypt);

    config_file.decrypt_databases()?;
    config_file.decrypt_callers()?;
    if config_file.count_encrypted_databases() == 0 && config_file.count_encrypted_callers() == 0 {
        config_file.clear_encryptions();
    }

    config_file.write_to(config_path)?;

    Ok(())
}

fn caller<T: AsRef<Path>>(config_path: T, args: &cli::SubCallerArgs) -> Result<()> {
    // read existing or create new config
    let mut config_file = if let Ok(config_file) = Config::read_from(&config_path) {
        verify_caller(&config_file)?;
        config_file
    } else {
        Config::new()
    };

    match &args.command {
        cli::CallerSubcommands::Add(caller_add_args) => {
            let caller = Caller {
                path: caller_add_args.path.clone(),
                uid: caller_add_args.uid,
                gid: caller_add_args.gid,
                canonicalize: caller_add_args.canonicalize,
            };
            if let Some(ref encryption) = caller_add_args.encrypt {
                // this will error if an existing encryption profile has already been configured for the
                // underlying hardware/etc
                // in this case user should decrypt the configuration first
                config_file.add_encryption(encryption)?;
            }
            config_file.add_caller(caller, caller_add_args.encrypt.is_some())?;
            config_file.write_to(config_path)
        }
        cli::CallerSubcommands::Me(caller_me_args) => {
            let caller = {
                let current_caller = CurrentCaller::new()?;
                #[cfg(unix)]
                let caller = Caller::from_current_caller(
                    &current_caller,
                    caller_me_args.no_uid,
                    caller_me_args.no_uid,
                    caller_me_args.canonicalize,
                );
                #[cfg(windows)]
                let caller =
                    Caller::from_current_caller(&current_caller, caller_me_args.canonicalize);
                println!(
                    "Gonna save current caller to allowed callers list:\n{}",
                    serde_json::to_string_pretty(&caller)?
                );
                prompt_for_confirmation()?;
                caller
            };
            if let Some(ref encryption) = caller_me_args.encrypt {
                // this will error if an existing encryption profile has already been configured for the
                // underlying hardware/etc
                // in this case user should decrypt the configuration first
                config_file.add_encryption(encryption)?;
            }
            config_file.add_caller(caller, caller_me_args.encrypt.is_some())?;
            config_file.write_to(config_path)
        }
        cli::CallerSubcommands::Clear(_) => {
            config_file.clear_callers();
            config_file.write_to(config_path)
        }
    }
}

fn verify_caller(config: &Config) -> Result<Option<CurrentCaller>> {
    if config.count_callers() == 0
        && (cfg!(not(feature = "strict-caller")) || config.count_databases() == 0)
    {
        info!(
            "Caller verification skipped as no caller profiles defined and strict-caller disabled"
        );
        return Ok(None);
    }
    let current_caller = CurrentCaller::new()?;
    let callers = config.get_callers()?;
    let matching_callers = callers
        .iter()
        .filter(|caller| current_caller.matches(caller));
    if matching_callers.count() == 0 {
        if config.count_callers() == 0 && cfg!(feature = "strict-caller") {
            warn!("No caller profiles defined. You must configure callers before databases when strict-caller feature is enabled");
        }
        info!(
            "Run `{}` to add current caller to allowed caller list",
            current_caller.command_to_add(config.count_encrypted_callers() > 0)
        );
        #[cfg(windows)]
        let error_message = format!(
            "{} is not allowed to call git-credential-keepassxc",
            current_caller.path.to_string_lossy()
        );
        #[cfg(not(windows))]
        let error_message = format!(
            "{} (uid={}, gid={}) is not allowed to call git-credential-keepassxc",
            current_caller.path.to_string_lossy(),
            current_caller.uid,
            current_caller.gid
        );
        Err(anyhow!(error_message))
    } else {
        Ok(Some(current_caller))
    }
}

/// Returns all entries from KeePassXC except for expired ones (which are not returned by KeePassXC
/// actually, but better to be safe than sorry)
fn get_logins_for<T: AsRef<str>>(
    config: &Config,
    client_id: T,
    url: T,
    filters: &EntryFilters,
    unlock_options: &Option<UnlockOptions>,
) -> Result<(Vec<LoginEntry>, String)> {
    let databases = associated_databases(config, client_id.as_ref(), unlock_options)?;
    let id_key_pairs: Vec<_> = databases
        .iter()
        .map(|d| (d.id.as_str(), d.pkey.as_str()))
        .collect();

    // ask KeePassXC for logins
    let gl_req = GetLoginsRequest::new(url.as_ref(), None, None, &id_key_pairs[..]);
    let (gl_resp, gl_resp_raw) = gl_req.send(client_id.as_ref(), false)?;

    let mut login_entries: Vec<_> = gl_resp
        .entries
        .into_iter()
        .filter(|e| e.expired.is_none() || !e.expired.as_ref().unwrap().0)
        .collect();
    info!("KeePassXC returned {} login(s)", login_entries.len());

    if filters.kph {
        let num_entries = login_entries.len();
        login_entries.retain(filter_kph);
        let num_filtered = num_entries - login_entries.len();
        if num_filtered > 0 {
            info!(
                "{} login(s) were filtered out due to having label KPH: git == false",
                num_filtered
            );
        }
    }
    {
        let num_entries = login_entries.len();
        login_entries.retain(|login_entry| {
            filter_group(login_entry, &filters.groups, filters.git_groups, &databases)
        });
        let num_filtered = num_entries - login_entries.len();
        if num_filtered > 0 {
            info!("{} login(s) were filtered out by group", num_filtered);
        }
    }

    Ok((login_entries, gl_resp_raw))
}

fn get_totp_for<T: AsRef<str>>(client_id: T, uuid: T) -> Result<GetTotpResponse> {
    let gt_req = GetTotpRequest::new(uuid.as_ref());
    let (mut gt_resp, _) = gt_req.send(client_id.as_ref(), false)?;
    gt_resp.uuid = Some(uuid.as_ref().to_owned());
    if gt_resp.success.is_some() && gt_resp.success.as_ref().unwrap().0 && !gt_resp.totp.is_empty()
    {
        Ok(gt_resp)
    } else {
        Err(anyhow!("Failed to get TOTP"))
    }
}

fn filter_kph(login_entry: &LoginEntry) -> bool {
    if let Some(ref string_fields) = login_entry.string_fields {
        let kph_false_fields = string_fields.iter().find(|m| {
            if let Some(v) = m.get("KPH: git") {
                v == "false"
            } else {
                false
            }
        });
        kph_false_fields.is_none()
    } else {
        true
    }
}

fn filter_group(
    login_entry: &LoginEntry,
    groups: &[String],
    git_groups: bool,
    databases: &[Database],
) -> bool {
    if let Some(ref login_entry_group) = login_entry.group {
        if !git_groups {
            return groups.is_empty() || groups.contains(login_entry_group);
        }
        let database_groups: Vec<&String> = databases.iter().map(|d| &d.group).collect();
        groups.contains(login_entry_group) || database_groups.contains(&login_entry_group)
    } else {
        if !groups.is_empty() || git_groups {
            warn!("Group filter(s) provided but no group info from KeePassXC (using KeePassXC < 2.6.0?)");
        }
        true
    }
}

fn get_logins<T, A>(
    config_path: T,
    unlock_options: &Option<UnlockOptions>,
    entry_filters: EntryFilters,
    args: &A,
) -> Result<()>
where
    T: AsRef<Path>,
    A: cli::GetOperation,
{
    let config = Config::read_from(config_path.as_ref())?;
    let _current_caller = verify_caller(&config)?;
    // read credential request
    let git_req = GitCredentialMessage::from_stdin()?;
    let url = git_req.get_url()?;

    #[cfg(feature = "notification")]
    {
        if let Some(current_caller) = _current_caller {
            use notify_rust::{Notification, Timeout};
            let notification = Notification::new()
                .summary("Credential request")
                .body(&format!(
                    "{} ({}) has requested credential for {}",
                    current_caller
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    current_caller.pid,
                    url
                ))
                .timeout(Timeout::Milliseconds(6000))
                .show();
            if let Err(e) = notification {
                warn!("Failed to show notification for credential request, {}", e);
            }
        }
    }

    // start session
    let (client_id, _, _) = start_session()?;

    let (mut login_entries, login_entries_raw) =
        get_logins_for(&config, &client_id, &url, &entry_filters, unlock_options)?;

    if args.raw() {
        // GetMode::PasswordAndTotp in raw mode is banned in CLI

        if args.get_mode() == GetMode::PasswordOnly {
            io::stdout().write_all(login_entries_raw.as_bytes())?;
        }
        if args.get_mode() == GetMode::TotpOnly {
            // this is a little hacky
            // they're not actually KeePassXC raw responses but serialised from our struct to
            // inject UUIDs
            // is there a better way to do this? use a HashMap?
            let totp_results: Vec<_> = login_entries
                .iter()
                .flat_map(|login| {
                    let totp = get_totp_for(&client_id, &login.uuid);
                    if let Err(ref e) = totp {
                        warn!(
                            "Failed to get TOTP for {} ({}), Caused by: {}",
                            login.name, login.uuid, e
                        );
                    }
                    totp.ok()
                })
                .collect();
            io::stdout().write_all(serde_json::to_string(&totp_results)?.as_bytes())?;
        }
        return Ok(());
    }

    if login_entries.is_empty() {
        return Err(anyhow!("No matching logins found"));
    }
    if login_entries.len() > 1 && git_req.username.is_some() {
        let username = git_req.username.as_ref().unwrap();
        let login_entries_name_matches: Vec<_> = login_entries
            .iter()
            .filter(|entry| entry.login == *username)
            .cloned()
            .collect();
        if !login_entries_name_matches.is_empty() {
            info!(
                "{} login(s) left after filtering by username",
                login_entries_name_matches.len()
            );
            login_entries = login_entries_name_matches;
        }
    }
    if login_entries.len() > 1 {
        warn!("More than 1 matching logins found, only the first one will be returned");
    }

    let login = login_entries.first().unwrap();
    let mut git_resp = git_req;

    // entry found handle TOTP now
    match args.get_mode() {
        GetMode::PasswordAndTotp => {
            if let Ok(totp) = get_totp_for(&client_id, &login.uuid) {
                git_resp.totp = Some(totp.totp);
            } else {
                error!("Failed to get TOTP");
            }
        }
        GetMode::TotpOnly => {
            git_resp.totp = Some(get_totp_for(&client_id, &login.uuid)?.totp);
        }
        _ => {}
    }

    if args.get_mode() != GetMode::TotpOnly {
        git_resp.username = Some(login.login.clone());
        git_resp.password = Some(login.password.clone());
    }

    if args.advanced_fields() {
        if let Some(ref login_entry_fields) = login.string_fields {
            if !login_entry_fields.is_empty() {
                git_resp.set_string_fields(login_entry_fields);
            }
        }
    }

    if args.json() {
        io::stdout().write_all(serde_json::to_string(&git_resp)?.as_bytes())?;
    } else {
        io::stdout().write_all(git_resp.to_string().as_bytes())?;
    }

    Ok(())
}

fn store_login<T: AsRef<Path>>(
    config_path: T,
    unlock_options: &Option<UnlockOptions>,
    entry_filters: EntryFilters,
    args: &cli::SubStoreArgs,
) -> Result<()> {
    let config = Config::read_from(config_path.as_ref())?;
    verify_caller(&config)?;
    // read credential request
    let git_req = GitCredentialMessage::from_stdin()?;
    let url = git_req.get_url()?;
    // start session
    let (client_id, _, _) = start_session()?;

    if git_req.username.is_none() {
        return Err(anyhow!("Username is missing"));
    }
    if git_req.password.is_none() {
        return Err(anyhow!("Password is missing"));
    }

    let login_entries = get_logins_for(&config, &client_id, &url, &entry_filters, unlock_options)
        .and_then(|(entries, _)| {
            let username = git_req.username.as_ref().unwrap();
            let entries: Vec<_> = entries
                .into_iter()
                .filter(|entry| entry.login == *username)
                .collect();
            info!(
                "{} login(s) left after filtering by username",
                entries.len()
            );
            if entries.is_empty() {
                // this Err is never used
                Err(anyhow!("Failed to find entry to update"))
            } else {
                Ok(entries)
            }
        });

    let sl_req = if let Ok(login_entries) = login_entries {
        if login_entries.len() == 1 {
            warn!("Existing login found, gonna update the entry");
        } else {
            warn!("More than 1 existing logins found, gonna update the first entry");
        }
        let login_entry = login_entries.first().unwrap();

        if &login_entry.login == git_req.username.as_ref().unwrap()
            && &login_entry.password == git_req.password.as_ref().unwrap()
        {
            // KeePassXC treats this as error, and Git sometimes does this as the operation should
            // be idempotent
            info!("No changes detected, ignoring request");
            return Ok(());
        }

        let databases = config.get_databases()?;
        if databases.len() > 1 {
            // how do I know which database it's from?
            error!(
                "Trying to update an existing login when multiple databases are configured, this is not implemented yet"
            );
            unimplemented!();
        }
        let database = databases.first().unwrap();
        SetLoginRequest::new(
            &url,
            &url,
            &database.id,
            &git_req.username.unwrap(),
            &git_req.password.unwrap(),
            Some(&database.group),
            Some(&database.group_uuid), // KeePassXC won't move the existing entry though
            Some(&login_entry.uuid),
        )
    } else {
        info!("No existing logins found, gonna create a new one");
        let databases = config.get_databases()?;
        if databases.len() > 1 {
            warn!(
                "More than 1 databases configured, gonna save the new login in the first database"
            );
        }
        let database = databases.first().unwrap();
        let (group, group_uuid) = if let Some(ref group) = args.create_in {
            let gg_req = GetDatabaseGroupsRequest::new();
            let (gg_resp, _) = gg_req.send(&client_id, false)?;
            let group_uuid = gg_resp
                .get_flat_groups()
                .iter()
                .filter(|g| g.name == group)
                .map(|g| g.uuid)
                .next()
                .ok_or_else(|| anyhow!("Failed to find group {group}"))?;
            (group.clone(), group_uuid.to_owned())
        } else {
            (database.group.clone(), database.group_uuid.clone())
        };
        SetLoginRequest::new(
            &url,
            &url,
            &database.id,
            &git_req.username.unwrap(),
            &git_req.password.unwrap(),
            Some(&group),
            Some(&group_uuid),
            None,
        )
    };
    let (sl_resp, _) = sl_req.send(&client_id, false)?;

    sl_resp.check(&sl_req.get_action())
}

fn erase_login() -> Result<()> {
    // Don't treat this as error as when server rejects a login Git may try to erase it. This is
    // not desirable since sometimes it's merely a configuration issue, e.g. a lot of Git servers
    // reject logins over HTTP(S) when SSH keys have been uploaded
    error!("KeePassXC doesn't allow erasing logins via socket at the time of writing");
    let _ = GitCredentialMessage::from_stdin()?;
    Ok(())
}

fn lock_database<T: AsRef<Path>>(config_path: T) -> Result<()> {
    let config = Config::read_from(config_path.as_ref())?;
    verify_caller(&config)?;
    // start session
    let (client_id, _, _) = start_session()?;

    let ld_req = LockDatabaseRequest::new();
    let (ld_resp, _) = ld_req.send(client_id, false)?;

    ld_resp.check(&ld_req.get_action())
}

fn get_groups<T: AsRef<Path>>(
    config_path: T,
    unlock_options: &Option<UnlockOptions>,
    args: &cli::SubGroupsArgs,
) -> Result<()> {
    let config = Config::read_from(config_path.as_ref())?;
    verify_caller(&config)?;
    // start session
    let (client_id, _, _) = start_session()?;

    let _ = associated_databases(&config, &client_id, unlock_options)?;

    let gg_req = GetDatabaseGroupsRequest::new();
    let (gg_resp, gg_resp_raw) = gg_req.send(client_id, false)?;

    if args.raw {
        io::stdout().write_all(gg_resp_raw.as_bytes())?;
    } else {
        let mut tw = TabWriter::new(io::stdout());
        tw.write_all("Parents\tName\tUUID\n".as_bytes())?;
        tw.write_all("--\t--\t--\n".as_bytes())?;
        let groups = gg_resp.get_flat_groups();
        for group in groups {
            let parents = group
                .parents
                .iter()
                .map(|p| {
                    if p.contains("->") {
                        format!("\"{}\"", p.replace('"', "\\\""))
                    } else {
                        p.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join(" -> ");
            tw.write_fmt(format_args!(
                "{}\t{}\t{}\n",
                parents, group.name, group.uuid
            ))?;
        }
        tw.flush()?;
    }

    Ok(())
}

fn generate_password<T: AsRef<Path>>(
    config_path: T,
    args: &cli::SubGeneratePasswordArgs,
) -> Result<()> {
    let config = Config::read_from(config_path.as_ref())?;
    verify_caller(&config)?;
    // start session
    let (client_id, _, _) = start_session()?;

    let (_, nonce_b64) = nacl_nonce();
    let gp_req = GeneratePasswordRequest::new(&client_id, &nonce_b64);
    let (gp_resp, _) = gp_req.send(&client_id, false)?;

    let git_resp = GitCredentialMessage {
        password: Some(gp_resp.password),
        ..Default::default()
    };

    if args.json {
        io::stdout().write_all(serde_json::to_string(&git_resp)?.as_bytes())?;
    } else {
        io::stdout().write_all(git_resp.to_string().as_bytes())?;
    }

    Ok(())
}

fn edit<T: AsRef<Path>>(config_path: T) -> Result<()> {
    const KNOWN_EDITORS: &[&str] = &["nvim", "vim", "kak", "vi", "nano", "ex"];
    let find_editor = || -> Option<String> {
        if let Ok(editor) = env::var("VISUAL") {
            debug!("Found editor {} via VISUAL environment variable", editor);
            return Some(editor);
        } else if let Ok(editor) = env::var("EDITOR") {
            debug!("Found editor {} via EDITOR environment variable", editor);
            return Some(editor);
        } else {
            for editor in KNOWN_EDITORS {
                if which::which(editor).is_ok() {
                    debug!("Found known editor {}", editor);
                    return Some(editor.to_string());
                }
            }
        }
        None
    };

    if let Some(editor) = find_editor() {
        println!(
            "Opening {} using {}",
            config_path.as_ref().to_string_lossy(),
            editor
        );
        let mut editor_process = Command::new(editor)
            .arg(config_path.as_ref())
            .spawn()
            .map_err(|e| anyhow!("Failed to open editor: {}", e))?;
        println!("Waiting user to finish...");
        editor_process.wait()?;
    } else {
        println!(
            "Failed to find an editor automatically. Go ahead and open {} in your favourite editor :)",
            config_path.as_ref().to_string_lossy()
        );
    }

    #[cfg(unix)]
    {
        let metadata = Path::metadata(config_path.as_ref());
        if let Ok(metadata) = metadata {
            if metadata.permissions().mode() & 0o377 > 0 {
                warn!("Permission of configuration file might be too open (suggested 0o400)");
            }
        }
    }

    Ok(())
}

fn real_main() -> Result<()> {
    #[cfg(all(target_os = "linux", not(debug_assertions)))]
    {
        prctl::set_dumpable(false)
            .or_else(|c| Err(anyhow!("Failed to disable dump, code: {}", c)))?;
    }

    let args = cli::MainArgs::parse();

    let level =
        Level::from_usize(std::cmp::min(6, args.verbose + 2) as usize).unwrap_or(Level::Error);
    let decorator = slog_term::TermDecorator::new().build();
    let drain = slog_term::FullFormat::new(decorator)
        .build()
        .filter_level(level)
        .fuse();
    let drain = std::sync::Mutex::new(drain).fuse();
    let logger = Logger::root(drain, slog::o!());
    LOGGER
        .set(logger)
        .map_err(|_| anyhow!("Failed to initialise logger"))?;

    #[cfg(all(target_os = "linux", not(debug_assertions)))]
    {
        if let Ok(dumpable) = prctl::get_dumpable() {
            if dumpable {
                error!("Failed to disable dump");
            } else {
                info!("Dump is disabled");
            }
        } else {
            error!("Failed to query dumpable status");
        }
    }

    let config_path = {
        if let Some(path) = &args.config {
            info!("Configuration file path is set to {} by user", path);
            PathBuf::from(path)
        } else {
            let base_dirs = directories_next::BaseDirs::new()
                .ok_or_else(|| anyhow!("Failed to initialise base_dirs"))?;
            base_dirs.config_dir().join(env!("CARGO_BIN_NAME"))
        }
    };
    if let Some(path) = &args.socket {
        info!("Socket path is set to {} by user", path);
        env::set_var(utils::socket::KEEPASS_SOCKET_ENVIRONMENT_VARIABLE, path);
    };
    if let Some(ref unlock_options) = args.unlock {
        info!(
            "Database unlock option is given by user: max retries {}, interval {}ms",
            unlock_options.max_retries, unlock_options.interval
        );
    }

    let main_entry_filters = args.entry_filters();

    debug!("Subcommand: {}", args.command.name());
    match &args.command {
        cli::Subcommands::Configure(configure_args) => configure(config_path, configure_args),
        cli::Subcommands::Edit(_) => edit(config_path),
        cli::Subcommands::Encrypt(encrypt_args) => encrypt(config_path, encrypt_args),
        cli::Subcommands::Decrypt(_) => decrypt(config_path),
        cli::Subcommands::Caller(caller_args) => caller(config_path, caller_args),
        cli::Subcommands::Get(get_args) => {
            let entry_filters = get_args.local_entry_filters(main_entry_filters);
            if entry_filters.has_non_default() && get_args.raw {
                Err(clap::Error::raw(
                    clap::ErrorKind::ArgumentConflict,
                    "Filter options (--group, --git-groups, --no-filter) cannot be used with --raw",
                ))?;
            }
            get_logins(config_path, &args.unlock, entry_filters, get_args)
        }
        cli::Subcommands::Totp(totp_args) => {
            let entry_filters = totp_args.local_entry_filters(main_entry_filters);
            if entry_filters.has_non_default() && totp_args.raw {
                Err(clap::Error::raw(
                    clap::ErrorKind::ArgumentConflict,
                    "Filter options (--group, --git-groups, --no-filter) cannot be used with --raw",
                ))?;
            }
            get_logins(config_path, &args.unlock, entry_filters, totp_args)
        }
        cli::Subcommands::Store(store_args) => {
            let entry_filters = store_args.local_entry_filters(main_entry_filters);
            store_login(config_path, &args.unlock, entry_filters, store_args)
        }
        cli::Subcommands::Erase(_) => erase_login(),
        cli::Subcommands::Lock(_) => lock_database(config_path),
        cli::Subcommands::Groups(groups_args) => get_groups(config_path, &args.unlock, groups_args),
        cli::Subcommands::GeneratePassword(generate_password_args) => {
            generate_password(config_path, generate_password_args)
        }
    }
}

fn main() {
    if let Err(e) = real_main() {
        let source = e
            .source()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        if LOGGER.get().is_some() {
            error!("{}, Caused by: {}", e, source);
        } else {
            // failed to initialise LOGGER
            println!("{e}, Caused by: {source}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "strict-caller")]
    fn test_00_verification_success_when_strict_caller_but_no_database() {
        let config = Config::new();
        assert!(verify_caller(&config).is_ok());
    }

    #[test]
    #[cfg(feature = "strict-caller")]
    fn test_01_verification_failure_when_strict_caller_and_database() {
        let mut config = Config::new();
        let database = Database {
            id: "test_01".to_string(),
            key: "".to_string(),
            pkey: "".to_string(),
            group: "".to_string(),
            group_uuid: "".to_string(),
        };
        config.add_database(database, false).unwrap();

        assert!(verify_caller(&config).is_err());
    }

    #[test]
    #[cfg(not(feature = "strict-caller"))]
    fn test_02_verification_success_when_database_but_no_strict_caller() {
        let mut config = Config::new();
        let database = Database {
            id: "test_02".to_string(),
            key: "".to_string(),
            pkey: "".to_string(),
            group: "".to_string(),
            group_uuid: "".to_string(),
        };
        config.add_database(database, false).unwrap();

        assert!(verify_caller(&config).is_ok());
    }
}
