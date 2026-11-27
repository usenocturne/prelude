use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";
const KERNEL_BUILD: &str = "/proc/sys/kernel/version";

pub fn invalidate_stale_gstreamer_registry() {
    let Some(registry) = std::env::var_os("GST_REGISTRY").map(PathBuf::from) else {
        return;
    };
    let kernel = match kernel_identity() {
        Ok(kernel) => kernel,
        Err(error) => {
            tracing::warn!(%error, "cannot read the running kernel build");
            return;
        }
    };
    match refresh(&registry, &kernel) {
        Ok(true) => tracing::info!(
            registry = %registry.display(),
            "discarded the GStreamer registry cached under a different kernel"
        ),
        Ok(false) => {}
        Err(error) => tracing::warn!(
            %error,
            registry = %registry.display(),
            "cannot refresh the GStreamer registry for this kernel"
        ),
    }
}

fn kernel_identity() -> io::Result<String> {
    Ok(format!(
        "{}\n{}",
        fs::read_to_string(KERNEL_RELEASE)?.trim(),
        fs::read_to_string(KERNEL_BUILD)?.trim()
    ))
}

fn stamp_path(registry: &Path) -> PathBuf {
    let mut path = OsString::from(registry.as_os_str());
    path.push(".kernel");
    PathBuf::from(path)
}

fn refresh(registry: &Path, kernel: &str) -> io::Result<bool> {
    let stamp = stamp_path(registry);
    if fs::read_to_string(&stamp).is_ok_and(|cached| cached == kernel) {
        return Ok(false);
    }
    let discarded = match fs::remove_file(registry) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    fs::write(&stamp, kernel)?;
    Ok(discarded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registry_from_another_kernel_is_discarded_once() {
        let directory = tempfile::tempdir().expect("directory");
        let registry = directory.path().join("gstreamer-registry.bin");
        fs::write(&registry, b"cached").expect("registry");

        assert!(refresh(&registry, "7.0.2\n#1 SMP new").expect("refresh"));
        assert!(!registry.exists());
        fs::write(&registry, b"rebuilt").expect("registry");
        assert!(!refresh(&registry, "7.0.2\n#1 SMP new").expect("refresh"));
        assert!(registry.exists());

        assert!(refresh(&registry, "7.0.2\n#1 SMP newer").expect("refresh"));
        assert!(!registry.exists());
    }

    #[test]
    fn a_missing_registry_only_records_the_kernel() {
        let directory = tempfile::tempdir().expect("directory");
        let registry = directory.path().join("gstreamer-registry.bin");
        assert!(!refresh(&registry, "kernel").expect("refresh"));
        assert_eq!(
            fs::read_to_string(stamp_path(&registry)).expect("stamp"),
            "kernel"
        );
    }
}
