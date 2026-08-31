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
        // The shell's cwd is passed as an env var; wasi-libc ignores
        // `initial_cwd`, so chdir explicitly so relative paths resolve in the
        // right directory.
        if let Ok(cwd) = std::env::var("CARSON_CWD") {
            let _ = std::env::set_current_dir(&cwd);
        }
        let code = dispatch(&util, args);
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        Ok(code)
    }
}

fn dispatch(util: &str, args: Vec<OsString>) -> u32 {
    let args = || -> std::vec::IntoIter<OsString> { args.clone().into_iter() };
    let code = match util {
        "base32" => uu_base32::uumain(args()),
        "base64" => uu_base64::uumain(args()),
        "basename" => uu_basename::uumain(args()),
        "cat" => uu_cat::uumain(args()),
        "cksum" => uu_cksum::uumain(args()),
        "comm" => uu_comm::uumain(args()),
        "cp" => uu_cp::uumain(args()),
        "csplit" => uu_csplit::uumain(args()),
        "cut" => uu_cut::uumain(args()),
        "date" => uu_date::uumain(args()),
        "dirname" => uu_dirname::uumain(args()),
        "expand" => uu_expand::uumain(args()),
        "factor" => uu_factor::uumain(args()),
        "fmt" => uu_fmt::uumain(args()),
        "fold" => uu_fold::uumain(args()),
        "head" => uu_head::uumain(args()),
        "join" => uu_join::uumain(args()),
        "link" => uu_link::uumain(args()),
        "ln" => uu_ln::uumain(args()),
        "ls" => uu_ls::uumain(args()),
        "md5sum" => uu_md5sum::uumain(args()),
        "mkdir" => uu_mkdir::uumain(args()),
        "mktemp" => uu_mktemp::uumain(args()),
        "mv" => uu_mv::uumain(args()),
        "nl" => uu_nl::uumain(args()),
        "numfmt" => uu_numfmt::uumain(args()),
        "od" => uu_od::uumain(args()),
        "paste" => uu_paste::uumain(args()),
        "pathchk" => uu_pathchk::uumain(args()),
        "pr" => uu_pr::uumain(args()),
        "printf" => uu_printf::uumain(args()),
        "ptx" => uu_ptx::uumain(args()),
        "pwd" => uu_pwd::uumain(args()),
        "readlink" => uu_readlink::uumain(args()),
        "realpath" => uu_realpath::uumain(args()),
        "rm" => uu_rm::uumain(args()),
        "rmdir" => uu_rmdir::uumain(args()),
        "seq" => uu_seq::uumain(args()),
        "sha1sum" => uu_sha1sum::uumain(args()),
        "sha224sum" => uu_sha224sum::uumain(args()),
        "sha256sum" => uu_sha256sum::uumain(args()),
        "sha384sum" => uu_sha384sum::uumain(args()),
        "sha512sum" => uu_sha512sum::uumain(args()),
        "shuf" => uu_shuf::uumain(args()),
        "sort" => uu_sort::uumain(args()),
        "split" => uu_split::uumain(args()),
        "sum" => uu_sum::uumain(args()),
        "tail" => uu_tail::uumain(args()),
        "tee" => uu_tee::uumain(args()),
        "touch" => uu_touch::uumain(args()),
        "tr" => uu_tr::uumain(args()),
        "truncate" => uu_truncate::uumain(args()),
        "tsort" => uu_tsort::uumain(args()),
        "unexpand" => uu_unexpand::uumain(args()),
        "uniq" => uu_uniq::uumain(args()),
        "wc" => uu_wc::uumain(args()),
        "yes" => uu_yes::uumain(args()),
        _ => {
            eprintln!("{util}: command not found");
            return 127;
        }
    };
    code.max(0) as u32
}

wit::export!(Runner with_types_in wit);
