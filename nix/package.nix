# Luthier, built from this repository.
#
# Only `luthier-cli` is built: `luthier-registry` is for maintaining the
# bench, not for using the manager. The test suite runs as the check phase —
# it is offline by design (the one HTTP server it starts is on localhost), so
# the sandbox is not a reason to skip it.
{
  lib,
  rustPlatform,
  installShellFiles,
  cmake,
  perl,
  cacert,
}:

let
  workspace = (lib.importTOML ../Cargo.toml).workspace.package;
in
rustPlatform.buildRustPackage {
  pname = "luthier";
  inherit (workspace) version;

  src = lib.fileset.toSource {
    root = ../.;
    # What the build and the suite read: the workspace, the bench the
    # validator tests against, and the schema a test compares to its source.
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../rustfmt.toml
      ../crates
      ../bench
      ../schemas
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  # aws-lc-sys, under rustls, builds its C with cmake and generates some of
  # it with perl.
  nativeBuildInputs = [
    installShellFiles
    cmake
    perl
  ];
  # cmake is a build tool for a dependency here, not this package's build
  # system; without this the cmake setup hook would try to configure the
  # workspace itself.
  dontUseCmakeConfigure = true;

  # The suite's local HTTP server is plain HTTP, but building a client at
  # all needs a CA store, and the sandbox has none.
  nativeCheckInputs = [ cacert ];

  cargoBuildFlags = [
    "--package"
    "luthier-cli"
  ];
  cargoTestFlags = [ "--workspace" ];

  # The binary writes its own man page and completions, so they cannot drift
  # from the arguments it actually accepts.
  postInstall = ''
    $out/bin/luthier man > luthier.1
    installManPage luthier.1
    installShellCompletion --cmd luthier \
      --bash <($out/bin/luthier completions bash) \
      --zsh <($out/bin/luthier completions zsh) \
      --fish <($out/bin/luthier completions fish)
  '';

  meta = {
    description = "Package manager for Linux audio plugins and sample libraries";
    longDescription = ''
      Installs CLAP, VST3 and LV2 plugins and the sample libraries that play
      in them, from the Open Audio Stack registry and a curated bench, into
      the directories hosts already scan. Nothing is ever run to install a
      package: it is downloaded, checked against its checksum, extracted and
      copied.
    '';
    homepage = "https://github.com/savashn/luthier";
    changelog = "https://github.com/savashn/luthier/blob/main/CHANGELOG.md";
    license = lib.licenses.lgpl21Plus;
    mainProgram = "luthier";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
