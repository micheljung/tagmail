{
  lib,
  stdenv,
  makeRustPlatform,
  rust-bin,
  nodejs,
  pnpm_12,
  pnpmConfigHook,
  fetchPnpmDeps,
}:

let
  root = ../..;
  inherit ((lib.importTOML (root + "/Cargo.toml")).workspace.package) version;

  # nixpkgs' rustc lags the toolchain the repository pins
  toolchain = rust-bin.fromRustupToolchainFile (root + "/rust-toolchain.toml");
  rustPlatform = makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };

  pnpm = pnpm_12;

  web = stdenv.mkDerivation (finalAttrs: {
    pname = "tagmail-web";
    inherit version;
    src = lib.fileset.toSource {
      inherit root;
      fileset = lib.fileset.unions [
        (root + "/package.json")
        (root + "/pnpm-lock.yaml")
        (root + "/pnpm-workspace.yaml")
        (root + "/packages")
        (root + "/web")
      ];
    };

    nativeBuildInputs = [
      nodejs
      pnpm
      pnpmConfigHook
    ];
    pnpmWorkspaces = [ "@tagmail/web..." ];
    pnpmDeps = fetchPnpmDeps {
      inherit (finalAttrs)
        pname
        version
        src
        pnpmWorkspaces
        ;
      inherit pnpm;
      fetcherVersion = 3;
      # Changes with pnpm-lock.yaml, see deploy/nix/README.md
      hash = "sha256-HYAfFlK87WIWriyv7/433vJPHi4DyyVLmAyUnAefY50=";
    };

    buildPhase = ''
      runHook preBuild
      pnpm --filter @tagmail/web exec svelte-kit sync
      pnpm --filter @tagmail/web build
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      cp -r web/build $out
      runHook postInstall
    '';
  });
in
rustPlatform.buildRustPackage {
  pname = "tagmail-server";
  inherit version;
  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.unions [
      (root + "/Cargo.toml")
      (root + "/Cargo.lock")
      (root + "/.cargo")
      (root + "/crates")
      (root + "/xtask")
      (root + "/desktop/src-tauri")
    ];
  };
  cargoLock.lockFile = root + "/Cargo.lock";
  cargoBuildFlags = [
    "-p"
    "tagmail-api-server"
  ];
  doCheck = false;

  # The server serves `web` next to its binary
  postInstall = ''
    ln -s ${web} $out/bin/web
  '';

  passthru = { inherit web; };

  meta = {
    description = "Self-hosted Gmail-like IMAP client: sync server and web app";
    homepage = "https://github.com/micheljung/tagmail";
    license = lib.licenses.agpl3Only;
    mainProgram = "tagmail-server";
    platforms = lib.platforms.linux;
  };
}
