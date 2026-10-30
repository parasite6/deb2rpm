//! Narrow compatibility checks for Debian ncurses symbol labels on Fedora.
use super::*;

const VERSION: &str = "NCURSES6_TINFO_5.0.19991023";
pub(super) const REQUIREMENT: &str = "libtinfo.so.6(NCURSES6_TINFO_5.0.19991023)(64bit)";

fn symbols_compatible(imports: &str, exports: &str) -> bool {
    let suffix = format!("@{VERSION}");
    let mut count = 0;
    for line in imports.lines().filter(|line| line.contains(&suffix)) {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 8 || !fields[7].ends_with(&suffix) {
            return false;
        }
        let Some(size) = fields[2].parse::<u64>().ok() else {
            return false;
        };
        let name = fields[7].trim_end_matches(&suffix);
        if !matches!(fields[3], "FUNC" | "OBJECT") || !matches!(fields[4], "GLOBAL" | "WEAK") {
            return false;
        }
        let found = exports.lines().any(|line| {
            let host: Vec<_> = line.split_whitespace().collect();
            host.len() >= 8
                && host[7] == name
                && host[6] != "UND"
                && matches!(host[4], "GLOBAL" | "WEAK")
                && matches!(host[5], "DEFAULT" | "PROTECTED")
                && host[3] == fields[3]
                && (fields[3] != "OBJECT" || host[2].parse::<u64>().ok() == Some(size))
        });
        if !found {
            return false;
        }
        count += 1;
    }
    count > 0
}

fn version_from_tinfo(text: &str) -> bool {
    let mut needs = false;
    let mut library = "";
    let mut found = false;
    for line in text.lines() {
        if line.starts_with("Version needs section") {
            needs = true;
        }
        if line.starts_with("Version definition section") {
            needs = false;
        }
        if !needs {
            continue;
        }
        if let Some((_, value)) = line.split_once("File: ") {
            library = value.split_whitespace().next().unwrap_or("");
        }
        if let Some((_, value)) = line.split_once("Name: ") {
            if value.split_whitespace().next() == Some(VERSION) {
                if library != "libtinfo.so.6" {
                    return false;
                }
                found = true;
            }
        }
    }
    found
}

fn inspect(path: &Path, flags: &[&str]) -> Option<String> {
    let mut args = vec!["LC_ALL=C", READELF];
    args.extend_from_slice(flags);
    args.push(path.to_str()?);
    let output = run("/usr/bin/env", &args, None).ok()?;
    output.ok.then_some(output.stdout)
}

// Only the established ABI-6 terminfo label on native Fedora x86_64 is
// eligible. No package code is executed. Every uncertain check keeps RPM's
// original requirement; this never manufactures a library capability.
pub(super) fn alternative(tree: &Path, arch: &str, excluded: &[String]) -> Option<String> {
    if arch != "x86_64" {
        return None;
    }
    let identity = [2, 1, 62, 0];
    let mut consumers = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            let kind = entry.file_type().ok()?;
            // A bundled library can override the system candidate at runtime.
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("libtinfo.so")
            {
                return None;
            }
            if kind.is_dir() {
                stack.push(path);
                continue;
            }
            if !kind.is_file() || elf_identity(&path).is_none() {
                continue;
            }
            let relative = format!("/{}", path.strip_prefix(tree).ok()?.to_str()?);
            if excluded.contains(&relative) {
                continue;
            }
            let versions = inspect(&path, &["-d", "-V", "--wide"])?;
            if versions
                .lines()
                .any(|line| line.contains("(SONAME)") && line.contains("[libtinfo.so.6]"))
            {
                return None;
            }
            if versions.contains(VERSION) {
                if !version_from_tinfo(&versions) {
                    return None;
                }
                if elf_identity(&path)? != identity {
                    return None;
                }
                consumers.push(path);
            }
        }
    }
    if consumers.is_empty() {
        return None;
    }
    let existing = run(RPM, &["--eval", "%{?__requires_exclude}"], None).ok()?;
    if !existing.ok || !existing.stdout.trim().is_empty() {
        return None;
    }
    let library = fs::canonicalize("/usr/lib64/libtinfo.so.6").ok()?;
    if elf_identity(&library)? != identity {
        return None;
    }
    let host = inspect(&library, &["-d", "-V", "--dyn-syms", "--wide"])?;
    if host.contains("Version definition section")
        || !host
            .lines()
            .any(|line| line.contains("(SONAME)") && line.contains("[libtinfo.so.6]"))
    {
        return None;
    }
    let provider = run(
        RPM,
        &[
            "-qf",
            "--qf",
            "%{NAME}\t%{ARCH}\t%{VERSION}\n",
            library.to_str()?,
        ],
        None,
    )
    .ok()?;
    if !provider.ok {
        return None;
    }
    let fields: Vec<_> = provider.stdout.trim().split('\t').collect();
    if fields.len() != 3
        || fields[0] != "ncurses-libs"
        || fields[1] != "x86_64"
        || fields[2].is_empty()
        || !fields[2].chars().all(|c| c.is_ascii_digit() || c == '.')
    {
        return None;
    }
    // Reject locally modified provider files before relying on its RPM name.
    if !run(RPM, &["-V", "--noscripts", "-f", library.to_str()?], None)
        .ok()?
        .ok
    {
        return None;
    }
    for consumer in &consumers {
        let imports = inspect(consumer, &["--dyn-syms", "--wide"])?;
        if !symbols_compatible(&imports, &host) {
            return None;
        }
    }
    info(&format!(
        "verified terminfo symbol compatibility for {} ELF consumer(s) against {}",
        consumers.len(),
        library.display()
    ));
    Some(format!(
        "({REQUIREMENT} or ncurses-libs(x86-64) >= {})",
        fields[2]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_requires_all_versioned_symbols_and_matching_data_sizes() {
        let imports = "49: 0000 0 FUNC GLOBAL DEFAULT UND tputs@NCURSES6_TINFO_5.0.19991023 (7)\n411: 00dad30 8 OBJECT GLOBAL DEFAULT 27 cur_term@NCURSES6_TINFO_5.0.19991023 (7)\n";
        let exports = "1: 001230 184 FUNC GLOBAL DEFAULT 5 tputs\n2: 002c80 8 OBJECT GLOBAL DEFAULT 26 cur_term\n";
        assert!(symbols_compatible(imports, exports));
        for bad in [
            exports.replace("tputs", "other"),
            exports.replace("8 OBJECT", "4 OBJECT"),
            exports.replace("DEFAULT 5", "HIDDEN 5"),
            exports.replace("DEFAULT 5", "DEFAULT UND"),
            exports.replace("tputs", "tputs@@OTHER_VERSION"),
            exports.replace("FUNC", "OBJECT"),
        ] {
            assert!(!symbols_compatible(imports, &bad), "{bad}");
        }
        assert!(!symbols_compatible("", exports));
        assert!(!symbols_compatible(
            &imports.replace("8 OBJECT", "bad OBJECT"),
            exports
        ));
    }
    #[test]
    fn version_labels_must_belong_to_the_expected_library() {
        let text = "Version needs section '.gnu.version_r' contains 1 entry:\n  File: libtinfo.so.6 Cnt: 1\n  Name: NCURSES6_TINFO_5.0.19991023 Flags: none Version: 7\n";
        assert!(version_from_tinfo(text));
        assert!(!version_from_tinfo(
            &text.replace("libtinfo.so.6", "other.so")
        ));
        assert!(!version_from_tinfo(
            &text.replace("Version needs section", "Version definition section")
        ));
        assert!(!version_from_tinfo(&text.replace(VERSION, "OTHER_VERSION")));
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn rpm_keeps_soname_and_adds_verified_provider_alternative() {
        let work = env::temp_dir().join(format!(
            "deb2rpm-tinfo-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let tree = work.join("payload");
        fs::create_dir_all(tree.join("opt/probe")).unwrap();
        let stub = work.join("libtinfo.so.6");
        fs::write(work.join("stub.c"), "int tgetnum(const char *s) { return 0; } int deb2rpm_missing_symbol(void) { return 0; }\n").unwrap();
        fs::write(
            work.join("version.map"),
            "NCURSES6_TINFO_5.0.19991023 { global: tgetnum; deb2rpm_missing_symbol; local: *; };\n",
        )
        .unwrap();
        assert!(Command::new("/usr/bin/gcc")
            .args(["-shared", "-fPIC", "-Wl,-soname,libtinfo.so.6"])
            .arg(format!(
                "-Wl,--version-script={}",
                work.join("version.map").display()
            ))
            .arg(work.join("stub.c"))
            .arg("-o")
            .arg(&stub)
            .status()
            .unwrap()
            .success());
        let compile = |name: &str, code: &str| {
            fs::write(work.join("consumer.c"), code).unwrap();
            assert!(Command::new("/usr/bin/gcc")
                .args(["-shared", "-fPIC", "-Wl,--no-as-needed"])
                .arg(work.join("consumer.c"))
                .arg(&stub)
                .arg("-o")
                .arg(tree.join("opt/probe").join(name))
                .status()
                .unwrap()
                .success());
        };
        compile(
            "consumer.so",
            "extern int tgetnum(const char *); int call(void) { return tgetnum(\"co\"); }\n",
        );
        let rich = alternative(&tree, "x86_64", &[])
            .expect("Fedora's native unversioned provider must pass checks");
        assert!(rich.starts_with(
            "(libtinfo.so.6(NCURSES6_TINFO_5.0.19991023)(64bit) or ncurses-libs(x86-64) >= "
        ));
        assert!(alternative(&tree, "aarch64", &[]).is_none());
        assert!(alternative(&tree, "x86_64", &["/opt/probe/consumer.so".into()]).is_none());
        compile("required.so", "extern int deb2rpm_missing_symbol(void); int call(void) { return deb2rpm_missing_symbol(); }\n");
        assert!(
            alternative(&tree, "x86_64", &[]).is_none(),
            "every consumer must be compatible"
        );
        fs::remove_file(tree.join("opt/probe/required.so")).unwrap();
        fs::write(tree.join("opt/probe/libtinfo.so.6"), b"bundled").unwrap();
        assert!(alternative(&tree, "x86_64", &[]).is_none());
        fs::remove_file(tree.join("opt/probe/libtinfo.so.6")).unwrap();
        let consumer = tree.join("opt/probe/consumer.so");
        let bytes = fs::read(&consumer).unwrap();
        let mut foreign = bytes.clone();
        foreign[18] = 183;
        fs::write(&consumer, foreign).unwrap();
        assert!(alternative(&tree, "x86_64", &[]).is_none());
        fs::write(&consumer, &bytes).unwrap();
        let spec = work.join("probe.spec");
        fs::write(&spec,"Name: tinfoprobe\nVersion: 1\nRelease: 1\nSummary: terminfo compatibility probe\nLicense: MIT\n%description\nProbe\n%files\n%defattr(-,root,root,-)\n/opt/probe/consumer.so\n").unwrap();
        let rpm = rpmbuild(&tree, &spec, "x86_64", &work, &[]).unwrap();
        let output = run(RPM, &["-qp", "--requires", rpm.to_str().unwrap()], None).unwrap();
        assert!(output.ok);
        assert!(
            output.stdout.lines().any(|line| line == rich),
            "{}",
            output.stdout
        );
        assert!(output
            .stdout
            .lines()
            .any(|line| line == "libtinfo.so.6()(64bit)"));
        assert!(!output.stdout.lines().any(|line| line == REQUIREMENT));
        let files = run(
            RPM,
            &["-qp", "--qf", "[%{FILENAMES}\n]", rpm.to_str().unwrap()],
            None,
        )
        .unwrap();
        assert_eq!(files.stdout.trim(), "/opt/probe/consumer.so");
        assert_eq!(fs::read(consumer).unwrap(), bytes);
        fs::remove_dir_all(work).unwrap();
    }
}
