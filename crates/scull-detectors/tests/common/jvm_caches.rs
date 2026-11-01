//! Go, Gradle, Maven and NuGet cache layouts as the tools write them (P5-DET-01).

#![allow(dead_code, clippy::unwrap_used)]

use scull_fixtures::Fixture;

/// A Go build cache: Go's `README`, `trim.txt` and `00`…`ff` buckets of `-a`/`-d` entries.
pub fn go_build_cache(fx: &mut Fixture, dir: &str) {
    fx.file(
        format!("{dir}/README"),
        "This directory holds cached build artifacts from the Go build system.\n",
    )
    .unwrap()
    .file(format!("{dir}/trim.txt"), "1730000000\n")
    .unwrap()
    .file_of_size(format!("{dir}/0a/0a1b2c3d-a"), 512)
    .unwrap()
    .file_of_size(format!("{dir}/ff/ff00aa11-d"), 4096)
    .unwrap();
}

/// A Go module cache: `cache/download` (zips, `.info`, `.mod`) and an extracted module.
pub fn go_modcache(fx: &mut Fixture, dir: &str) {
    let dl = format!("{dir}/cache/download/github.com/pkg/errors/@v");
    fx.file_of_size(format!("{dl}/v0.9.1.zip"), 4096)
        .unwrap()
        .file(format!("{dl}/v0.9.1.info"), "{\"Version\":\"v0.9.1\"}")
        .unwrap()
        .file(format!("{dl}/v0.9.1.mod"), "module github.com/pkg/errors\n")
        .unwrap()
        .file_of_size(
            format!("{dir}/github.com/pkg/errors@v0.9.1/errors.go"),
            2048,
        )
        .unwrap();
}

/// Gradle's `caches/`: downloaded modules, transforms, the local build cache, a version folder.
pub fn gradle_caches(fx: &mut Fixture, dir: &str) {
    fx.file_of_size(
        format!("{dir}/modules-2/files-2.1/com.google.guava/guava/33.0.0-jre/abc/guava.jar"),
        4096,
    )
    .unwrap()
    .file(format!("{dir}/modules-2/modules-2.lock"), "")
    .unwrap()
    .file_of_size(format!("{dir}/transforms-4/0f1e/transformed/x.jar"), 1024)
    .unwrap()
    .file_of_size(format!("{dir}/build-cache-1/0123abcd"), 1024)
    .unwrap()
    .file(format!("{dir}/8.10/kotlin-dsl/accessors.txt"), "x")
    .unwrap();
}

/// A Maven local repository with one artifact.
pub fn maven_repository(fx: &mut Fixture, dir: &str) {
    let a = format!("{dir}/org/apache/commons/commons-lang3/3.14.0");
    fx.file_of_size(format!("{a}/commons-lang3-3.14.0.jar"), 4096)
        .unwrap()
        .file(format!("{a}/commons-lang3-3.14.0.pom"), "<project/>")
        .unwrap()
        .file(format!("{a}/_remote.repositories"), "central\n")
        .unwrap();
}

/// A NuGet global packages folder with one package (`<id>/<version>/.nupkg.metadata`).
pub fn nuget_packages(fx: &mut Fixture, dir: &str) {
    let p = format!("{dir}/newtonsoft.json/13.0.3");
    fx.file(
        format!("{p}/.nupkg.metadata"),
        "{\"version\":2,\"source\":\"https://api.nuget.org/v3/index.json\"}",
    )
    .unwrap()
    .file_of_size(format!("{p}/newtonsoft.json.13.0.3.nupkg"), 4096)
    .unwrap()
    .file_of_size(format!("{p}/lib/net6.0/Newtonsoft.Json.dll"), 2048)
    .unwrap();
}
