{
  config,
  lib,
  ...
}:

let
  cfg = config.services.tagmail;
  inherit (lib) mkOption types;

  isLoopback = a: lib.hasPrefix "127." a || a == "::1";
  listen =
    if lib.hasInfix ":" cfg.listen.address then
      "[${cfg.listen.address}]:${toString cfg.listen.port}"
    else
      "${cfg.listen.address}:${toString cfg.listen.port}";
  stateDir = lib.removePrefix "/var/lib/" cfg.dataDir;
  usesStateDirectory = lib.hasPrefix "/var/lib/" cfg.dataDir;
in
{
  options.services.tagmail = {
    enable = lib.mkEnableOption "Tagmail, a self-hosted Gmail-like IMAP client";

    package = mkOption {
      type = types.package;
      defaultText = lib.literalMD "`tagmail-server` from the Tagmail flake";
      description = "The tagmail-server package, with the web app in `bin/web`.";
    };

    listen = {
      address = mkOption {
        type = types.str;
        default = "127.0.0.1";
        description = ''
          IP address to listen on. A non-loopback address needs an `https`
          {option}`services.tagmail.publicOrigin`, because the server does not
          terminate TLS.
        '';
      };
      port = mkOption {
        type = types.port;
        default = 7878;
        description = "TCP port to listen on.";
      };
    };

    publicOrigin = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "https://mail.example.com";
      description = ''
        Origin browsers use to reach Tagmail, normally a TLS-terminating
        reverse proxy. Used for CSRF origin checks, passkeys, `Secure`
        cookies and pairing links. `null` means `http://localhost:<port>`.
      '';
    };

    dataDir = mkOption {
      type = types.str;
      default = "/var/lib/tagmail";
      description = ''
        Directory for `config.db`, `cache.db` and attachments. Below
        `/var/lib` it is managed with `StateDirectory=`, elsewhere it is
        created with tmpfiles.
      '';
    };

    masterKeyFile = mkOption {
      type = types.str;
      example = "/run/secrets/tagmail-master-key";
      description = ''
        File with the master key that encrypts IMAP and SMTP passwords: 32
        raw bytes, 64 hex digits or base64 of 32 bytes (`openssl rand -hex
        32`). Passed to the service with `LoadCredential=`, so it can stay
        readable by root only. Give it as a string, not a Nix path, so it is
        never copied into the world-readable Nix store, and keep it outside
        {option}`services.tagmail.dataDir`.
      '';
    };

    trustedProxies = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "127.0.0.1" ];
      description = ''
        Reverse proxy addresses or CIDR ranges whose `X-Forwarded-For` is
        trusted for login rate limits.
      '';
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [
        "--poll-interval"
        "60"
      ];
      description = "Extra arguments for `tagmail-server`, see `tagmail-server --help`.";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = false;
      description = "Open {option}`services.tagmail.listen.port` in the firewall.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion =
          isLoopback cfg.listen.address
          || (cfg.publicOrigin != null && lib.hasPrefix "https://" cfg.publicOrigin)
          || lib.elem "--insecure-http" cfg.extraArgs;
        message = ''
          services.tagmail: listening on ${cfg.listen.address} needs an https
          publicOrigin (TLS terminated by a reverse proxy); tagmail-server
          refuses to serve plain HTTP on a non-loopback address.
        '';
      }
      {
        assertion = !lib.hasPrefix builtins.storeDir cfg.masterKeyFile;
        message = "services.tagmail.masterKeyFile must not be in the Nix store.";
      }
      {
        assertion = lib.hasPrefix "/" cfg.dataDir && lib.hasPrefix "/" cfg.masterKeyFile;
        message = "services.tagmail.dataDir and masterKeyFile must be absolute paths.";
      }
    ];

    users.users.tagmail = {
      isSystemUser = true;
      group = "tagmail";
    };
    users.groups.tagmail = { };

    systemd.tmpfiles.rules = lib.optional (
      !usesStateDirectory
    ) "d ${cfg.dataDir} 0700 tagmail tagmail -";

    networking.firewall.allowedTCPPorts = lib.optional cfg.openFirewall cfg.listen.port;

    systemd.services.tagmail = {
      description = "Tagmail server";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = lib.escapeShellArgs (
          [
            (lib.getExe cfg.package)
            "--listen"
            listen
            "--data-dir"
            cfg.dataDir
          ]
          ++ lib.optionals (cfg.publicOrigin != null) [
            "--public-origin"
            cfg.publicOrigin
          ]
          ++ lib.concatMap (p: [
            "--trusted-proxy"
            p
          ]) cfg.trustedProxies
          ++ cfg.extraArgs
        );
        # Found by the server as $CREDENTIALS_DIRECTORY/master.key
        LoadCredential = [ "master.key:${cfg.masterKeyFile}" ];
        User = "tagmail";
        Group = "tagmail";
        StateDirectory = lib.mkIf usesStateDirectory stateDir;
        StateDirectoryMode = "0700";
        ReadWritePaths = lib.mkIf (!usesStateDirectory) [ cfg.dataDir ];
        Restart = "always";
        RestartSec = 10;

        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictAddressFamilies = [
          "AF_UNIX"
          "AF_INET"
          "AF_INET6"
        ];
        RestrictNamespaces = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallFilter = [ "@system-service" ];
        SystemCallErrorNumber = "EPERM";
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
        UMask = "0077";
      };
    };
  };
}
