# You can search the availability of a certain library here, https://conan.io/center/
# After adding the library, you can add it to the target using
#
# - `CONAN_PKG::library-name` (if you use this, you don't need to add the header files)
# - or ${CONAN_LIBS_LIBRARY-name} (it needs to be upper case)
#
# The header files can be added to a target using ${CONAN_INCLUDE_DIRS_LIBRARY-NAME}
from conan import ConanFile

class JaspConanConfig(ConanFile):
    settings = "os", "compiler", "build_type", "arch"
    generators = "CMakeToolchain", "CMakeDeps"
    options = {"syntax_interface_only": [True, False]}
    default_options = {
        "syntax_interface_only": False,
    }

    def requirements(self):
        # The excision aftermath (2026-09-02) — the diet:
        #   - boost            died: the last uses (lexical_cast/iequals/ends_with/
        #                     replace_all/uuid/null_sink) were replaced with QString/std/C++20
        #                     equivalents; find_package(Boost) is gone from Libraries.cmake.
        #   - sqlite3          died with DatabaseInterface (the excision, Cut 3).
        #   - gmp / mpfr       died: zero references anywhere in the build (no find_package,
        #                     no link, no include) — cargo cult from the pre-conanfile.txt era.
        #   - freexl / librdata   died with the importers (Cut 2), bison with readstat,
        #     brotli              died 2026-09-02: it was macOS packaging glue for WebEngine's
        #                         libbrotlicommon.dylib (QTBUG-100686), but the install line was
        #                         commented out and nothing ever linked a Brotli target. If conan's
        #                         libarchive pulls brotli transitively it manages it itself.
        # Still here: libarchive (ExtractArchive), libsodium (secret store), and the
        # compression/TLS set (zlib/zstd/openssl/libiconv) that conan's libarchive and the
        # Windows/macOS packaging graphs may pull — trim those only with a Win/mac check.
        self.requires("libiconv/1.18", force=True)
        self.requires("zlib/1.3.1")
        self.requires("libarchive/3.8.1")
        self.requires("zstd/1.5.7")
        self.requires("openssl/3.4.1")

        if not self.options.syntax_interface_only:
            # jsoncpp is vendored in Common/json/ so Conan's copy is not linked,
            # but keep it here for the full build to avoid unexpected Conan graph changes
            self.requires("jsoncpp/1.9.6")
            self.requires("libsodium/1.0.20")

    def build_requirements(self):
        self.tool_requires("cmake/3.30.0")
