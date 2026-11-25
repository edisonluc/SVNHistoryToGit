pub mod dump_parser;
pub mod inspector;

use std::path::Path;

use crate::config::TaskConfig;
use crate::credential::ResolvedSecret;
use crate::process::CommandSpec;

/// Build common non-interactive SVN CLI arguments without leaking passwords in argv.
pub fn build_svn_command(
    program: &str,
    subcommand_args: Vec<String>,
    config: &TaskConfig,
    secret: Option<&ResolvedSecret>,
    svn_config_dir: Option<&Path>,
) -> CommandSpec {
    let mut args = subcommand_args;
    args.push("--non-interactive".to_string());

    if config.trust_self_signed_cert {
        args.push("--trust-server-cert-failures=unknown-ca,cn-mismatch,expired,not-yet-valid,other".to_string());
    }

    if let Some(dir) = svn_config_dir {
        args.push("--config-dir".to_string());
        args.push(dir.to_string_lossy().to_string());
    }

    let username = secret
        .map(|s| s.username.as_str())
        .or(config.svn_username.as_deref())
        .filter(|u| !u.trim().is_empty());

    if let Some(user) = username {
        args.push("--username".to_string());
        args.push(user.to_string());
    }

    let mut spec = CommandSpec::new(program, args);
    if let Some(sec) = secret {
        if !sec.secret.is_empty() {
            spec.args.push("--password-from-stdin".to_string());
            spec.args.push("--no-auth-cache".to_string());
            // Do NOT append bare `\n`: Windows TortoiseSVN does not strip bare LF from --password-from-stdin
            spec.stdin_data = Some(sec.secret.as_bytes().to_vec());
        }
    }
    spec
}
