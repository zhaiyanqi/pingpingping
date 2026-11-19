fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=packaging/pingpingping.rc");
        println!("cargo:rerun-if-changed=packaging/pingpingping.ico");
        embed_resource::compile(
            "packaging/pingpingping.rc",
            embed_resource::ParamsIncludeDirs(["packaging"]),
        )
        .manifest_required()
        .expect("failed to embed the Windows app icon");
    }
}
