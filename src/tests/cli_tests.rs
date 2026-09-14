use crate::cli::should_track;

#[test]
fn accepts_every_tracked_executable_directly_and_through_sudo() {
    let commands = [
        "cargo", "make", "gmake", "go", "npm", "npx", "pnpm", "pnpx", "yarn", "bun", "gradle",
        "gradlew", "mvn", "mvnw",
    ];
    for command in commands {
        assert!(should_track(command), "{command}");
        assert!(should_track(&format!("{command} test --flag")), "{command}");
        assert!(should_track(&format!("sudo {command}")), "sudo {command}");
    }
}

#[test]
fn accepts_foreground_command_lines() {
    for command in [
        "  cargo test  ",
        "cargo test | tee output",
        "cargo test || echo failed",
        "cargo test && echo passed",
        "cargo test; echo done",
        "cargo>out",
        "cargo 2>&1",
        "cargo <&0",
        "cargo &>out",
        "cargo 'one & two'",
        "cargo \"one &! two\"",
        r"cargo one\&two",
        r"cargo \& later",
        r"cargo $'one\' & two'",
        "cargo $((flags & 1))",
        "cargo $(sleep 1 & wait)",
        "cargo `(sleep 1 & wait)`",
    ] {
        assert!(should_track(command), "{command:?}");
    }
}

#[test]
fn rejects_nonliteral_or_untracked_prefixes() {
    for command in [
        "",
        "   ",
        "Cargo test",
        "cargo-watch",
        "mvnn",
        "FOO=1 cargo test",
        "/usr/bin/make",
        "./gradlew",
        "../mvnw",
        "command cargo test",
        "env cargo test",
        "time cargo test",
        "'cargo' test",
        "\"cargo\" test",
        r"car\go test",
        "sudo",
        "sudo -E cargo test",
        "sudo -- cargo test",
        "sudo FOO=1 cargo test",
        "sudo /usr/bin/cargo test",
        "sudo 'cargo' test",
        "sudocargo test",
    ] {
        assert!(!should_track(command), "{command:?}");
    }
}

#[test]
fn rejects_top_level_asynchronous_operators_anywhere() {
    for command in [
        "& cargo test",
        "cargo&",
        "cargo &",
        "cargo&!",
        "cargo &|",
        "cargo | tee out &",
        "cargo && echo done &",
        "cargo; echo later &!",
        "cargo & echo later",
    ] {
        assert!(!should_track(command), "{command:?}");
    }
}

#[test]
fn rejects_malformed_quoting_and_escaping() {
    for command in [
        "cargo 'unterminated",
        "cargo \"unterminated",
        "cargo trailing\\",
        r"cargo $'unterminated\'",
        "cargo `unterminated",
        r"cargo \$'one\' & two'",
        "cargo (unterminated",
        "cargo unexpected)",
    ] {
        assert!(!should_track(command), "{command:?}");
    }
}
