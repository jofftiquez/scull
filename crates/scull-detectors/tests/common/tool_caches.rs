//! Long-tail tool cache layouts as the tools write them (P5-DET-05), and their default
//! locations per OS (home is `home/` in the fixture, no environment).

#![allow(dead_code, clippy::unwrap_used)]

use scull_detectors::caches::CacheTool;
use scull_detectors::rules::Platform as Os;
use scull_fixtures::Fixture;

/// The long-tail tools, in [`CacheTool::ALL`] order.
pub const LONG_TAIL_TOOLS: [CacheTool; 17] = [
    CacheTool::Ccache,
    CacheTool::Conan,
    CacheTool::Vcpkg,
    CacheTool::PlatformIo,
    CacheTool::Arduino,
    CacheTool::EspIdf,
    CacheTool::SwiftPm,
    CacheTool::CocoaPods,
    CacheTool::Coursier,
    CacheTool::Deno,
    CacheTool::Zig,
    CacheTool::Playwright,
    CacheTool::Cypress,
    CacheTool::Puppeteer,
    CacheTool::Electron,
    CacheTool::Homebrew,
    CacheTool::Nix,
];

/// Build `tool`'s cache layout at `dir` (relative to the fixture root).
pub fn build(fx: &mut Fixture, tool: CacheTool, dir: &str) {
    let f = |fx: &mut Fixture, rel: &str, size: u64| {
        fx.file_of_size(format!("{dir}/{rel}"), size).unwrap();
    };
    match tool {
        CacheTool::Ccache => {
            f(fx, "0/1/23456789abcdefR", 2048);
            f(fx, "a/b/cdef0123456789M", 1024);
            fx.file(format!("{dir}/ccache.conf"), "max_size = 5.0G\n")
                .unwrap();
        }
        CacheTool::Conan => {
            f(fx, "cache.sqlite3", 4096);
            f(fx, "zlib1a2b3c4d5e6f7/s/zlib.c", 2048);
            f(fx, "b/zlibf6e5d4c3b2a1/p/lib/libz.a", 4096);
        }
        CacheTool::Vcpkg => {
            f(
                fx,
                "3f/3f0a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4.zip",
                4096,
            );
        }
        CacheTool::PlatformIo => {
            f(fx, "db.data", 128);
            f(
                fx,
                "downloads/a1b2c3/framework-arduinoespressif32.tar.gz",
                4096,
            );
            f(fx, "http/4f/4f5e6d", 512);
        }
        CacheTool::Arduino => {
            f(fx, "packages/esp32-3.0.4.zip", 4096);
            f(fx, "libraries/Servo-1.2.1.zip", 1024);
        }
        CacheTool::EspIdf => {
            f(
                fx,
                "xtensa-esp-elf-13.2.0_20240530-x86_64-linux-gnu.tar.xz",
                4096,
            );
            f(fx, "cmake-3.24.0-linux-x86_64.tar.gz", 2048);
        }
        CacheTool::SwiftPm => {
            f(fx, "repositories/swift-argument-parser-0123abcd/HEAD", 64);
            f(fx, "manifests/manifest.db", 1024);
        }
        CacheTool::CocoaPods => {
            f(
                fx,
                "Pods/Release/Alamofire/5.9.1-abc12/Source/AF.swift",
                2048,
            );
            f(fx, "Pods/Specs/Release/Alamofire/5.9.podspec.json", 512);
            fx.file(format!("{dir}/Pods/VERSION"), "1.15.2").unwrap();
        }
        CacheTool::Coursier => {
            f(
                fx,
                "https/repo1.maven.org/maven2/org/typelevel/cats-core_3/2.12.0/cats-core_3-2.12.0.jar",
                4096,
            );
        }
        CacheTool::Deno => {
            f(fx, "deps/https/deno.land/abcdef0123", 1024);
            f(fx, "npm/registry.npmjs.org/chalk/5.3.0/index.js", 2048);
            f(fx, "gen/file/abc.js", 512);
        }
        CacheTool::Zig => {
            f(fx, "o/0123456789abcdef0123456789abcdef/libfoo.a", 4096);
            f(fx, "h/0123456789abcdef.txt", 64);
            f(fx, "p/1220abcdef/build.zig.zon", 128);
        }
        CacheTool::Playwright => {
            f(fx, "chromium-1140/chrome-linux/chrome", 4096);
            f(fx, "ffmpeg-1010/ffmpeg-linux", 1024);
            fx.file(format!("{dir}/.links/abc"), "/proj").unwrap();
        }
        CacheTool::Cypress => {
            f(fx, "13.6.0/Cypress/Cypress", 4096);
            f(fx, "12.17.4/Cypress/Cypress", 4096);
        }
        CacheTool::Puppeteer => {
            f(fx, "chrome/linux-127.0.6533.88/chrome-linux64/chrome", 4096);
            f(fx, "chrome-headless-shell/linux-127.0.6533.88/shell", 2048);
        }
        CacheTool::Electron => {
            f(
                fx,
                "4d3c9a8b7e6f5a4b3c2d1e0f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a0b/electron-v31.0.0-linux-x64.zip",
                4096,
            );
        }
        CacheTool::Homebrew => {
            f(
                fx,
                "downloads/0123abcd--wget-1.24.5.arm64_sonoma.bottle.tar.gz",
                4096,
            );
            f(fx, "api/formula.jws.json", 1024);
        }
        CacheTool::Nix => {
            f(
                fx,
                "0c5wdpx2rjjv3mbvsqgbdbm1w2rs1a3l-glibc-2.39/lib/libc.so.6",
                4096,
            );
            f(
                fx,
                "1aazwq8k8xjzp4ywxb5h1q5bi5cr9w2h-hello-2.12.1/bin/hello",
                1024,
            );
            fx.dir(format!("{dir}/.links")).unwrap();
        }
        other => panic!("{other:?} is not a long-tail tool"),
    }
}

/// Default cache folders per OS of the long-tail tools (Nix needs Nix's environment, so it has
/// no default here; SwiftPM has none on Windows, CocoaPods only on macOS, Homebrew none on
/// Windows).
pub fn default_layout(os: Os) -> Vec<(CacheTool, &'static str)> {
    match os {
        Os::Linux => vec![
            (CacheTool::Ccache, "home/.cache/ccache"),
            (CacheTool::Conan, "home/.conan2/p"),
            (CacheTool::Vcpkg, "home/.cache/vcpkg/archives"),
            (CacheTool::PlatformIo, "home/.platformio/.cache"),
            (CacheTool::Arduino, "home/.arduino15/staging"),
            (CacheTool::EspIdf, "home/.espressif/dist"),
            (CacheTool::SwiftPm, "home/.cache/org.swift.swiftpm"),
            (CacheTool::Coursier, "home/.cache/coursier/v1"),
            (CacheTool::Deno, "home/.cache/deno"),
            (CacheTool::Zig, "home/.cache/zig"),
            (CacheTool::Playwright, "home/.cache/ms-playwright"),
            (CacheTool::Cypress, "home/.cache/Cypress"),
            (CacheTool::Puppeteer, "home/.cache/puppeteer"),
            (CacheTool::Electron, "home/.cache/electron"),
            (CacheTool::Homebrew, "home/.cache/Homebrew"),
        ],
        Os::Macos => vec![
            (CacheTool::Ccache, "home/Library/Caches/ccache"),
            (CacheTool::Conan, "home/.conan2/p"),
            (CacheTool::Vcpkg, "home/.cache/vcpkg/archives"),
            (CacheTool::PlatformIo, "home/.platformio/.cache"),
            (CacheTool::Arduino, "home/Library/Arduino15/staging"),
            (CacheTool::EspIdf, "home/.espressif/dist"),
            (CacheTool::SwiftPm, "home/Library/Caches/org.swift.swiftpm"),
            (CacheTool::CocoaPods, "home/Library/Caches/CocoaPods"),
            (CacheTool::Coursier, "home/Library/Caches/Coursier/v1"),
            (CacheTool::Deno, "home/Library/Caches/deno"),
            (CacheTool::Zig, "home/.cache/zig"),
            (CacheTool::Playwright, "home/Library/Caches/ms-playwright"),
            (CacheTool::Cypress, "home/Library/Caches/Cypress"),
            (CacheTool::Puppeteer, "home/.cache/puppeteer"),
            (CacheTool::Electron, "home/Library/Caches/electron"),
            (CacheTool::Homebrew, "home/Library/Caches/Homebrew"),
        ],
        Os::Windows => vec![
            (CacheTool::Ccache, "home/AppData/Local/ccache"),
            (CacheTool::Conan, "home/.conan2/p"),
            (CacheTool::Vcpkg, "home/AppData/Local/vcpkg/archives"),
            (CacheTool::PlatformIo, "home/.platformio/.cache"),
            (CacheTool::Arduino, "home/AppData/Local/Arduino15/staging"),
            (CacheTool::EspIdf, "home/.espressif/dist"),
            (CacheTool::Coursier, "home/AppData/Local/Coursier/Cache/v1"),
            (CacheTool::Deno, "home/AppData/Local/deno"),
            (CacheTool::Zig, "home/AppData/Local/zig"),
            (CacheTool::Playwright, "home/AppData/Local/ms-playwright"),
            (CacheTool::Cypress, "home/AppData/Local/Cypress/Cache"),
            (CacheTool::Puppeteer, "home/.cache/puppeteer"),
            (CacheTool::Electron, "home/AppData/Local/electron/Cache"),
        ],
    }
}
