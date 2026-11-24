# Nix and NixOS

The repository is a flake. It provides:

| Output | Content |
|--------|---------|
| `packages.<system>.tagmail-server` (also `default`) | `bin/tagmail-server` with the web app in `bin/web`, where the server finds it |
| `packages.<system>.tagmail-web` | The built web app alone |
| `nixosModules.tagmail` (also `default`) | The `services.tagmail` NixOS module |
| `overlays.default` | Adds `tagmail-server` to `pkgs` |
| `checks.x86_64-linux.nixos-test` | NixOS VM test: setup, login and sync from Dovecot |

Systems: `x86_64-linux`, `aarch64-linux`.

## Using the flake

Try the server without installing it:

```sh
nix run github:micheljung/tagmail -- --help
```

Add it to a NixOS configuration:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    tagmail = {
      url = "github:micheljung/tagmail";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, tagmail, ... }: {
    nixosConfigurations.mail = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        tagmail.nixosModules.default
        {
          services.tagmail = {
            enable = true;
            publicOrigin = "https://mail.example.com";
            masterKeyFile = "/run/secrets/tagmail-master-key";
            trustedProxies = [ "127.0.0.1" ];
          };

          services.caddy = {
            enable = true;
            virtualHosts."mail.example.com".extraConfig = ''
              reverse_proxy 127.0.0.1:7878
            '';
          };
        }
      ];
    };
  };
}
```

`inputs.nixpkgs.follows` builds Tagmail with your nixpkgs. The flake is
tested with the nixpkgs in its `flake.lock`; drop the `follows` line if
your nixpkgs is too old to build it.

The server does not terminate TLS: keep it on a loopback address and put a
reverse proxy in front. The module refuses a non-loopback
`listen.address` without an `https` `publicOrigin`, as the server does.

The service is `tagmail.service`, running as the system user `tagmail`.
On first start it logs a one-time setup token:

```sh
journalctl -u tagmail.service | grep 'setup token'
```

Open `publicOrigin` and enter the token with the owner password.

## Module options

All options are under `services.tagmail`.

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Run `tagmail.service` |
| `package` | package | the flake's `tagmail-server` | Server package, with the web app in `bin/web` |
| `listen.address` | string | `"127.0.0.1"` | IP address to listen on. Non-loopback needs an `https` `publicOrigin` |
| `listen.port` | port | `7878` | TCP port |
| `publicOrigin` | null or string | `null` (`http://localhost:<port>`) | Origin browsers use, e.g. `https://mail.example.com`. Used for CSRF origin checks, passkeys, `Secure` cookies and pairing links |
| `dataDir` | string | `"/var/lib/tagmail"` | `config.db`, `cache.db`, uploads and outbox. Below `/var/lib` it is a systemd `StateDirectory`, elsewhere it is created with tmpfiles (mode 0700) |
| `masterKeyFile` | string | required | Master key file, see [Master key](#master-key) |
| `trustedProxies` | list of strings | `[ ]` | Proxy addresses or CIDR ranges whose `X-Forwarded-For` is used for login rate limits |
| `extraArgs` | list of strings | `[ ]` | More `tagmail-server` arguments (`tagmail-server --help`), e.g. `[ "--poll-interval" "60" ]` |
| `openFirewall` | bool | `false` | Open `listen.port` in the firewall. Not needed behind a local reverse proxy |

The unit runs with systemd sandboxing (`ProtectSystem=strict`, no
capabilities, `@system-service` system calls, `MemoryDenyWriteExecute`,
`UMask=0077`); only `dataDir` is writable.

## Master key

The master key encrypts the IMAP and SMTP passwords in `config.db`. The
module passes `masterKeyFile` to the service with `LoadCredential=`, so the
file can stay readable by root only. Give it as a string, never as a Nix
path such as `./master.key`, which would copy the key into the
world-readable Nix store (the module rejects store paths). Keep it outside
`dataDir`.

Create one (32 bytes as 64 hex digits):

```sh
sudo install -d -m 0700 /etc/tagmail
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/tagmail/master.key'
```

or manage it with sops-nix or agenix and point `masterKeyFile` at the
decrypted secret. Back it up together with `dataDir`
([backup](../../docs/backup.md)): without it, the stored account passwords are lost.

## Updating hashes

Rust dependencies are read from `Cargo.lock` directly; nothing to update.

The web app's pnpm dependencies are a fixed-output derivation whose hash
is in `deploy/nix/package.nix` (`pnpmDeps.hash`). After every change to
`pnpm-lock.yaml` it must be updated:

```sh
nix build .#tagmail-web.pnpmDeps -L
```

fails with `hash mismatch` and prints `got: sha256-…`; put that value in
`package.nix`. The `Nix` workflow runs this build first and reports a stale
hash as its own error.

`nix flake update` updates nixpkgs and rust-overlay. The Rust toolchain
follows `rust-toolchain.toml`.

## VM test

```sh
nix build .#checks.x86_64-linux.nixos-test -L
```

boots one VM with `services.tagmail` and Dovecot (plain IMAP on localhost,
one user with a seeded INBOX message), reads the setup token from the
journal, sets the owner password and logs in through the HTTP API, adds the
Dovecot account and waits until the seeded message is in the inbox. It
needs KVM to finish in a few minutes; without KVM it falls back to slow
emulation. The test passes `--insecure-plain-imap` because its Dovecot has
no TLS; do not use that flag with real accounts.

`nix flake check -L` builds the packages and runs the test; the `Nix`
workflow runs it for every pull request touching the build inputs.
