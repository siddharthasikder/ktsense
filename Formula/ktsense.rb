class Ktsense < Formula
  # One source of truth for the release the four assets come from. Homebrew scans the version out of
  # the URL, so declaring `version` as well is redundant and `brew audit --strict` rejects it.
  RELEASE = "0.1.0".freeze

  desc "Agent-first Kotlin code understanding CLI and MCP server (wraps kmp-lsp)"
  homepage "https://github.com/OWNER/ktsense"
  license "MIT"

  # The four sha256 values are placeholders: no release has been published yet, so no real artifact
  # hash exists. They are deliberately one repeated non-hash so an install fails loudly instead of
  # looking verified. scripts/bump-formula.sh (KT-44) rewrites them from the release .sha256 files.
  on_macos do
    on_arm do
      url "https://github.com/OWNER/ktsense/releases/download/v#{RELEASE}/ktsense-#{RELEASE}-aarch64-apple-darwin.tar.gz"
      sha256 "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    end
    on_intel do
      url "https://github.com/OWNER/ktsense/releases/download/v#{RELEASE}/ktsense-#{RELEASE}-x86_64-apple-darwin.tar.gz"
      sha256 "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/OWNER/ktsense/releases/download/v#{RELEASE}/ktsense-#{RELEASE}-aarch64-unknown-linux-musl.tar.gz"
      sha256 "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    end
    on_intel do
      url "https://github.com/OWNER/ktsense/releases/download/v#{RELEASE}/ktsense-#{RELEASE}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    end
  end

  def install
    # bin/ beside libexec/ is the archive's self-location contract: the binary resolves its engine at
    # <exe-dir>/../libexec/kmp-lsp, which only holds if both keep their relative places here.
    bin.install "bin/ktsense"

    # Whatever the archive ships under libexec/, never a named file. x86_64-apple-darwin carries no
    # kmp-jar-indexer: upstream kmp-lsp 0.26.0 ships an arm64 sidecar in its Intel tarball, so the
    # release omits it rather than mislabel it (KT-41).
    libexec.install Dir["libexec/*"]

    pkgshare.install "SKILL.md"
    # LICENSE and LICENSE.kmp-lsp are not installed here on purpose. Homebrew copies archive-root
    # metafiles into the prefix itself after install (build.rb install_metafiles, and Metafiles.copy?
    # treats both names as licenses), so installing them again would only move them out of its reach.
  end

  test do
    (testpath/"Hello.kt").write "class Greeter(val name: String) { fun greet(): String = \"hi\" }\n"
    assert_match "fun greet(): String", shell_output("#{bin}/ktsense outline #{testpath}/Hello.kt")
    assert_match version.to_s, shell_output("#{bin}/ktsense --version")
  end
end
