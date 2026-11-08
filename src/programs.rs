//! Which words name programs on this machine. Shell parsing here is heuristic, so a
//! value can land where a program name is expected; a verb class or hook label
//! prints only a word that names a program, so such a value never reaches the output.

use std::collections::HashSet;
use std::fs::DirEntry;
use std::path::{Path, PathBuf};

/// Shell builtins and keywords, PowerShell and `cmd` aliases, common commands and
/// common PowerShell cmdlets, known without a lookup, lower-case.
#[rustfmt::skip]
const COMMON: &[&str] = &[
    // POSIX shell builtins and keywords
    ".", ":", "[", "[[", "alias", "bg", "break", "builtin", "case", "cd", "command",
    "continue", "declare", "disown", "do", "done", "echo", "elif", "else", "esac", "eval",
    "exec", "exit", "export", "false", "fg", "fi", "for", "function", "getopts", "hash",
    "history", "if", "jobs", "kill", "let", "local", "popd", "printf", "pushd", "pwd",
    "read", "readonly", "return", "select", "set", "shift", "source", "test", "then",
    "time", "trap", "true", "type", "ulimit", "umask", "unalias", "unset", "until", "wait",
    "while",
    // PowerShell and cmd aliases and keywords
    "%", "?", "call", "cat", "chdir", "clear", "cls", "copy", "cp", "del", "dir", "erase",
    "foreach", "gc", "gci", "gcm", "gi", "gl", "gm", "gp", "gps", "gsv", "iex", "irm", "iwr",
    "ls", "md", "mkdir", "mv", "ni", "param", "ps", "rd", "rem", "ren", "ri", "rm", "rmdir",
    "sl", "sleep", "sls", "sort", "start", "switch", "tee", "throw", "try", "where", "write",
    // Common commands
    "apt", "apt-get", "awk", "base64", "basename", "bash", "bat", "bun", "bunx", "cargo",
    "chmod", "chown", "claude", "cmd", "cmp", "code", "codex", "comm", "column", "curl",
    "cut", "date", "deno", "df", "diff", "dig", "dirname", "docker", "dotnet", "du", "env",
    "expr", "fd", "file", "find", "findstr", "fish", "free", "gh", "git", "go", "grep",
    "gzip", "head", "hostname", "id", "java", "jest", "journalctl", "jq", "less", "ln",
    "lsof", "make", "md5sum", "more", "nice", "nl", "node", "nohup", "npm", "npx",
    "nslookup", "od", "patch", "pgrep", "ping", "pip", "pip3", "pkill", "pnpm", "poetry",
    "powershell", "printenv", "pwsh", "py", "pytest", "python", "python3", "readlink",
    "realpath", "rg", "robocopy", "rsync", "rustc", "rustup", "scp", "sed", "seq", "sh",
    "sha256sum", "split", "sqlite3", "ssh", "stat", "sudo", "systemctl", "tail", "tar",
    "taskkill", "tasklist", "timeout", "top", "touch", "tr", "tree", "uname", "uniq",
    "unzip", "uv", "vitest", "watch", "wc", "wget", "whoami", "winget", "wsl", "xargs",
    "xxd", "yarn", "zip", "zsh",
    // Common PowerShell cmdlets
    "add-content", "clear-content", "compress-archive", "convertfrom-json",
    "convertto-json", "copy-item", "expand-archive", "export-csv", "foreach-object",
    "format-list", "format-table", "get-acl", "get-childitem", "get-ciminstance",
    "get-command", "get-content", "get-date", "get-filehash", "get-help", "get-item",
    "get-itemproperty", "get-location", "get-member", "get-module", "get-process",
    "get-service", "get-variable", "group-object", "import-csv", "import-module",
    "invoke-command", "invoke-expression", "invoke-restmethod", "invoke-webrequest",
    "join-path", "measure-object", "move-item", "new-item", "new-object", "out-file",
    "out-host", "out-null", "out-string", "pop-location", "push-location", "remove-item",
    "rename-item", "resolve-path", "select-object", "select-string", "set-content",
    "set-item", "set-itemproperty", "set-location", "set-variable", "sort-object",
    "split-path", "start-process", "start-sleep", "stop-process", "tee-object",
    "test-connection", "test-path", "wait-process", "where-object", "write-error",
    "write-host", "write-output", "write-warning",
];

/// Extensions a program or script with a folder in its name may have.
const RUNNABLE: [&str; 10] = [
    "exe", "cmd", "bat", "com", "ps1", "sh", "js", "mjs", "cjs", "py",
];

/// The programs in the folders on `PATH`.
#[derive(Debug, Default)]
pub struct Programs {
    /// Runnable file names: on Windows lower-case, with no extension or one in
    /// `PATHEXT`; elsewhere files with an execute bit.
    names: HashSet<String>,
    /// `PATHEXT` extensions on Windows, lower-case with the dot; none elsewhere.
    exts: Vec<String>,
}

impl Programs {
    /// Reads the folders on `PATH` once and, on Windows, `PATHEXT`.
    #[must_use]
    pub fn from_env() -> Self {
        let exts = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
                .split(';')
                .filter(|e| !e.is_empty())
                .map(str::to_lowercase)
                .collect()
        } else {
            Vec::new()
        };
        let dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
            .unwrap_or_default();
        Self::scan(&dirs, exts)
    }

    /// Indexes the runnable files in `dirs`. An empty or relative entry, as a
    /// trailing `;` gives, would search the current folder, so it is skipped.
    fn scan(dirs: &[PathBuf], exts: Vec<String>) -> Self {
        let mut names = HashSet::new();
        for dir in dirs.iter().filter(|d| d.is_absolute()) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                if let Some(name) = e.file_name().to_str()
                    && runnable(&e, name, &exts)
                {
                    names.insert(if cfg!(windows) {
                        name.to_lowercase()
                    } else {
                        name.to_owned()
                    });
                }
            }
        }
        Self { names, exts }
    }

    /// Whether `word`, a command's first word or a hook's script as written, names a
    /// program: a common command or cmdlet, a program on `PATH`, or an existing file
    /// with a program or script extension.
    #[must_use]
    pub fn has(&self, word: &str) -> bool {
        let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
        if name.len() < word.len() {
            let path = Path::new(word);
            let ext_runnable = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| RUNNABLE.contains(&e.to_lowercase().as_str()));
            if ext_runnable && path.is_file() {
                return true;
            }
        }
        let lower = name.to_lowercase();
        let base = lower.strip_suffix(".exe").unwrap_or(&lower);
        COMMON.contains(&base) || self.on_path(name)
    }

    fn on_path(&self, name: &str) -> bool {
        let key = if cfg!(windows) {
            name.to_lowercase()
        } else {
            name.to_owned()
        };
        self.names.contains(&key)
            || self
                .exts
                .iter()
                .any(|e| self.names.contains(&format!("{key}{e}")))
    }
}

/// A file Windows can run by name: no extension, or one in `PATHEXT`.
#[cfg(windows)]
fn runnable(e: &DirEntry, name: &str, exts: &[String]) -> bool {
    let lower = name.to_lowercase();
    let ext_ok = Path::new(&lower)
        .extension()
        .and_then(|x| x.to_str())
        .is_none_or(|x| exts.iter().any(|e| e.strip_prefix('.') == Some(x)));
    ext_ok && e.file_type().is_ok_and(|t| !t.is_dir())
}

/// A file with an execute bit, following symbolic links.
#[cfg(unix)]
fn runnable(e: &DirEntry, _name: &str, _exts: &[String]) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(e.path()).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_commands_and_cmdlets_are_known() {
        let p = Programs::default();
        for w in ["git", "GIT.EXE", "Get-Content", "select-string", "cd", "%"] {
            assert!(p.has(w), "{w}");
        }
        for w in ["sentinel", "update-acmepayroll", "get-", "zqxclient.pdf"] {
            assert!(!p.has(w), "{w}");
        }
    }

    #[test]
    fn path_lookups_need_a_runnable_file() {
        let dir = std::env::temp_dir().join(format!("shaphan-programs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("docs")).expect("temp folder");
        let tool = dir.join(if cfg!(windows) {
            "mytool.exe"
        } else {
            "mytool"
        });
        std::fs::write(&tool, "").expect("stub");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755))
                .expect("execute bit");
        }
        std::fs::write(dir.join("readme.md"), "").expect("stub");
        std::fs::write(dir.join("docs").join("client.pdf"), "").expect("stub");
        let exts = if cfg!(windows) {
            vec![".exe".to_owned()]
        } else {
            Vec::new()
        };
        let p = Programs::scan(&[dir.clone(), PathBuf::new()], exts);
        let full = |n: &str| dir.join(n).display().to_string();
        assert!(p.has("mytool"));
        assert!(p.has(&full("mytool")));
        assert!(!p.has("readme.md"));
        assert!(!p.has(&full("docs/client.pdf")));
        assert!(!p.has("missing"));
        std::fs::remove_dir_all(&dir).expect("remove the temporary folder");
    }
}
