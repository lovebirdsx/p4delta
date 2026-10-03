fn main() {
    println!("cargo:rerun-if-changed=p4delta.rc");
    println!("cargo:rerun-if-changed=p4delta.exe.manifest");

    #[cfg(windows)]
    {
        // VERSIONINFO 里的版本号从 Cargo.toml 算出来、编译期注入，仓库里不再有第二份
        // 手写的副本。manifest 的 assemblyIdentity version 是 XML 属性，宏注入不进去，
        // 那一份由 src/cli.rs 的用例拦着。
        let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is always set");
        let mut numbers = version.split('.').map(|part| {
            part.parse::<u16>()
                .unwrap_or_else(|_| panic!("Cargo.toml version {version} is not a numeric version"))
        });
        let (major, minor, patch) = (
            numbers.next().unwrap_or(0),
            numbers.next().unwrap_or(0),
            numbers.next().unwrap_or(0),
        );

        embed_resource::compile(
            "p4delta.rc",
            [
                format!("VER_FILE={major},{minor},{patch},0"),
                // VERSIONINFO 的字符串值必须以 NUL 结尾，rc.exe 不会替你补。
                format!(r#"VER_STRING="{version}\0""#),
            ],
        );
    }
}
