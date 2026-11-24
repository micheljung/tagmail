{
  description = "Tagmail: self-hosted Gmail-like IMAP client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
      forAllSystems = f: lib.genAttrs systems (system: f (pkgsFor system));
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          tagmail-server = pkgs.callPackage ./deploy/nix/package.nix { };
        in
        {
          inherit tagmail-server;
          tagmail-web = tagmail-server.web;
          default = tagmail-server;
        }
      );

      overlays.default = final: _prev: {
        tagmail-server = self.packages.${final.stdenv.hostPlatform.system}.tagmail-server;
      };

      nixosModules = {
        tagmail =
          { lib, pkgs, ... }:
          {
            imports = [ ./deploy/nix/module.nix ];
            services.tagmail.package =
              lib.mkDefault
                self.packages.${pkgs.stdenv.hostPlatform.system}.tagmail-server;
          };
        default = self.nixosModules.tagmail;
      };

      checks.x86_64-linux.nixos-test = (pkgsFor "x86_64-linux").testers.runNixOSTest (
        import ./deploy/nix/nixos-test.nix { tagmailModule = self.nixosModules.default; }
      );

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
