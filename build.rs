fn main() {
    // Only embed manifest on Windows
    #[cfg(windows)]
    {
        embed_resource::compile("p4delta.rc", embed_resource::NONE);
    }
}
