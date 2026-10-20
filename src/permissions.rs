//! Who has said yes. A plugin declares in its file what its logic wants to
//! touch of the system, but declaring it is not having it: **whoever uses it approves it**, once,
//! and it is stored outside the plugin, together with the fingerprint of its logic and of what
//! it asked for. If the plugin changes —its code, or what it asks for—, the approval no longer holds.
//!
//! The scene you open yourself does not go through here: opening it is already deciding, like
//! running a script. What comes in through an `import` is what has to be looked at.

use crate::scene::{Permissions, Plugin};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What runs anything it is told: asking for it is asking for everything.
const INTERPRETERS: [&str; 12] = ["sh", "bash", "zsh", "fish", "dash", "python", "python3", "perl", "ruby", "node", "lua", "env"];

fn approvals_file() -> PathBuf {
    let dir = crate::platform::config_dir();
    let file = dir.join("permissions.json");
    // Until 2026-09-25 it was called `permisos.json`: move it once, or every
    // plugin already approved would ask again.
    let old = dir.join("permisos.json");
    if !file.exists() && old.exists() {
        let _ = std::fs::rename(&old, &file);
    }
    file
}

/// FNV-1a, 64-bit: small, and the same everywhere and in every version.
fn fnv(bytes: impl Iterator<Item = u8>) -> u64 {
    bytes.fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3))
}

/// What an approval depends on: the plugin's code and what it asks for.
pub fn fingerprint(p: &Plugin) -> String {
    let code = std::fs::read(&p.logic).unwrap_or_default();
    let asks = format!("|run:{}|services:{}", p.permissions.commands.join(","), p.permissions.services.join(","));
    format!("{:016x}", fnv(code.into_iter().chain(asks.bytes())))
}

fn approved() -> BTreeMap<String, String> {
    std::fs::read_to_string(approvals_file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn key(p: &Plugin) -> String {
    p.logic.canonicalize().unwrap_or_else(|_| p.logic.clone()).to_string_lossy().into_owned()
}

/// A plugin that asks for nothing does not need anyone to say yes to it.
pub fn is_approved(p: &Plugin) -> bool {
    p.permissions == Permissions::default() || approved().get(&key(p)) == Some(&fingerprint(p))
}

/// The permissions it really runs with: its own if they are approved; if not, none.
pub fn effective(p: &Plugin) -> Permissions {
    if is_approved(p) { p.permissions.clone() } else { Permissions::default() }
}

pub fn approve(p: &Plugin) -> Result<(), String> {
    let mut all = approved();
    all.insert(key(p), fingerprint(p));
    let path = approvals_file();
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&path, serde_json::to_string_pretty(&all).unwrap()).map_err(|e| format!("{}: {e}", path.display()))
}

/// What it asks for, said so that it is understood, and warning about what asking for everything is.
pub fn describe(p: &Permissions) -> String {
    let list = |l: &[String], flag: &dyn Fn(&str) -> bool| match l {
        [] => "none".to_owned(),
        _ => l.iter().map(|x| if flag(x) { format!("{x} ⚠") } else { x.clone() }).collect::<Vec<_>>().join(", "),
    };
    // Bare: it runs whatever it is told. With its script (`"node usage.js"`), only that.
    let is_interpreter = |o: &str| !o.contains(' ') && INTERPRETERS.contains(&o.rsplit('/').next().unwrap_or(o));
    // `say:*` tells any scene any of `--say`'s commands, `quit` among them.
    let says_to_all = |x: &str| x == "say:*";
    let mut s = format!("commands: {} · services: {}", list(&p.commands, &is_interpreter), list(&p.services, &says_to_all));
    if p.commands.iter().any(|o| is_interpreter(o)) {
        s.push_str(" · ⚠ an interpreter runs whatever it is told: that is asking for everything");
    }
    if p.services.iter().any(|x| says_to_all(x)) {
        s.push_str(" · ⚠ say:* tells any running scene anything --say can, quit included");
    }
    s
}

/// `pleamar --approve scene.plm`: shows what each plugin of that scene asks for, and asks.
/// With `yes_to_all`, it does not ask (for scripts; use it knowing what is being approved).
pub fn ask(scene: &str, yes_to_all: bool) -> i32 {
    let e = match crate::scenes::from_file::read(scene) {
        Ok(e) => e,
        Err(m) => {
            eprintln!("{m}");
            return 1;
        }
    };
    let pending: Vec<&Plugin> = e.plugins.iter().filter(|p| !is_approved(p)).collect();
    if e.plugins.is_empty() {
        println!("{scene} uses no plugins: there is nothing to approve.");
        return 0;
    }
    for p in e.plugins.iter().filter(|p| is_approved(p)) {
        println!("✓ '{}' · {} · {}", p.name, p.logic.display(), if p.permissions == Permissions::default() { "asks for nothing".to_owned() } else { format!("already approved · {}", describe(&p.permissions)) });
    }
    let mut denied = 0;
    for p in pending {
        println!("\n? Plugin '{}' ({}) wants:\n    {}", p.name, p.logic.display(), describe(&p.permissions));
        let yes = yes_to_all || {
            print!("  Approve it? [y/N] ");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            matches!(line.trim().to_lowercase().as_str(), "s" | "si" | "sí" | "y" | "yes")
        };
        if yes {
            match approve(p) {
                Ok(()) => println!("  approved. If its logic or what it asks for changes, it will have to be approved again."),
                Err(m) => {
                    eprintln!("  could not save it: {m}");
                    return 1;
                }
            }
        } else {
            println!("  not approved: it will run, but unable to touch anything of the system.");
            denied += 1;
        }
    }
    if denied > 0 { 2 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::{Permissions, describe};

    #[test]
    fn telling_every_scene_is_flagged_and_telling_one_is_not() {
        let p = |services: &[&str]| Permissions { commands: Vec::new(), services: services.iter().map(|s| s.to_string()).collect() };
        let one = describe(&p(&["say:bar"]));
        assert!(!one.contains('⚠'), "{one}");
        let all = describe(&p(&["audio", "say:*"]));
        assert!(all.contains("say:* ⚠"), "{all}");
        assert!(all.contains("⚠ say:* tells any running scene"), "{all}");
    }
}
