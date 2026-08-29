#![no_main]
#![crate_type = "cdylib"]

mod wit {
    wit_bindgen::generate!({
        path: "wit-shell",
        world: "carson:shell/coreutils-world",
    });
}

use std::ffi::OsString;
use std::io::Write;

use wit::exports::carson::shell::coreutils::Guest;

/// Dispatches one coreutils util. argv[0] is the utility name (as the shell
/// invoked it); env, cwd and stdio are configured by the host via WASI. Each
/// `uumain` already converts its result into an exit code.
struct Runner;

impl Guest for Runner {
    fn run() -> Result<u32, String> {
        let args: Vec<OsString> = std::env::args_os().collect();
        let util = args
            .first()
            .map(|a| a.to_string_lossy().into_owned())
            .unwrap_or_default();
        let code = dispatch(&util, args);
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        Ok(code)
    }
}

fn dispatch(util: &str, args: Vec<OsString>) -> u32 {
    let args = || -> std::vec::IntoIter<OsString> { args.clone().into_iter() };
    let code = match util {
        "ls" => uu_ls::uumain(args()),
        "cat" => uu_cat::uumain(args()),
        "cp" => uu_cp::uumain(args()),
        "mv" => uu_mv::uumain(args()),
        "rm" => uu_rm::uumain(args()),
        "mkdir" => uu_mkdir::uumain(args()),
        "touch" => uu_touch::uumain(args()),
        "date" => uu_date::uumain(args()),
        _ => {
            eprintln!("{util}: command not found");
            return 127;
        }
    };
    code.max(0) as u32
}

wit::export!(Runner with_types_in wit);