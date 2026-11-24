{ tagmailModule }:

let
  port = 7878;
  subject = "Hello from the NixOS test";
in
{
  name = "tagmail";

  nodes.machine =
    {
      config,
      lib,
      pkgs,
      ...
    }:
    let
      dovecot = config.services.dovecot2.package;
      seed = pkgs.writeShellScriptBin "seed-mail" ''
        exec ${dovecot}/libexec/dovecot/deliver -d alice <<MAIL
        From: bob@localhost
        To: alice@localhost
        Subject: ${subject}
        Message-ID: <nixos-test@localhost>
        Date: Tue, 24 Nov 2026 12:00:00 +0000

        Seeded before Tagmail starts.
        MAIL
      '';
    in
    {
      imports = [ tagmailModule ];

      virtualisation.memorySize = 2048;

      users.users.alice = {
        isNormalUser = true;
        password = "foobar";
      };

      services.dovecot2 = {
        enable = true;
        enablePAM = true;
        settings = {
          dovecot_config_version = dovecot.version;
          dovecot_storage_version = dovecot.version;
          protocols = [ "imap" ];
          mail_driver = "maildir";
          mail_path = "~/Maildir";
        };
      };

      services.tagmail = {
        enable = true;
        listen.port = port;
        masterKeyFile = "/etc/tagmail/master.key";
        # Dovecot here speaks plain IMAP; never on a server with real accounts
        extraArgs = [
          "--insecure-plain-imap"
          "--poll-interval"
          "2"
        ];
      };
      # Started by the test script once the key exists
      systemd.services.tagmail.wantedBy = lib.mkForce [ ];

      environment.systemPackages = [
        pkgs.curl
        pkgs.openssl
        seed
      ];
    };

  testScript = ''
    import base64
    import json
    import shlex

    base = "http://127.0.0.1:${toString port}"

    def api(method, path, body=None, jar="/tmp/jar", expect=200):
        cmd = (
            f"curl -sS -o /tmp/resp -w '%{{http_code}}' -b {jar} -c {jar}"
            f" -X {method} -H 'X-Requested-With: tagmail'"
        )
        if body is not None:
            cmd += " -H 'Content-Type: application/json' --data-raw " + shlex.quote(json.dumps(body))
        status = int(machine.succeed(cmd + " " + shlex.quote(base + path)))
        raw = base64.b64decode(machine.succeed("base64 -w0 /tmp/resp"))
        assert status == expect, f"{method} {path}: {status} {raw!r}"
        return raw

    def decode_ids(data):
        ids, prev, value, shift = [], 0, 0, 0
        for byte in data:
            value |= (byte & 0x7F) << shift
            shift += 7
            if byte & 0x80 == 0:
                prev += (value >> 1) ^ -(value & 1)
                ids.append(prev)
                value, shift = 0, 0
        return ids

    machine.wait_for_unit("dovecot.service")
    machine.succeed("seed-mail")

    machine.succeed("install -d -m 0700 /etc/tagmail")
    machine.succeed("openssl rand -hex 32 > /etc/tagmail/master.key")
    machine.systemctl("start tagmail.service")
    machine.wait_for_unit("tagmail.service")
    machine.wait_for_open_port(${toString port})
    machine.succeed(f"curl -sf {base}/healthz")
    machine.succeed(f"curl -sf {base}/ | grep -q '<html'")
    api("GET", "/api/version")

    with subtest("setup with the token from the journal"):
        token = machine.wait_until_succeeds(
            "journalctl -u tagmail.service -o cat"
            " | grep -o 'one-time setup token [A-Za-z0-9_-]*' | tail -n1 | cut -d' ' -f4"
            " | grep ."
        ).strip()
        api("GET", "/api/accounts", expect=401)
        api("POST", "/api/auth/setup", {"token": "wrong", "password": "correct horse battery"}, expect=403)
        api("POST", "/api/auth/setup", {"token": token, "password": "correct horse battery"}, expect=201)

    with subtest("login"):
        api("POST", "/api/auth/login", {"password": "wrong"}, jar="/tmp/login", expect=401)
        api("POST", "/api/auth/login", {"password": "correct horse battery"}, jar="/tmp/login")
        api("GET", "/api/auth/session", jar="/tmp/login")

    with subtest("sync the seeded message from Dovecot"):
        api("POST", "/api/accounts", {
            "name": "alice",
            "imapHost": "127.0.0.1",
            "imapPort": 143,
            "imapTls": "starttls",
            "smtpHost": "127.0.0.1",
            "smtpPort": 25,
            "smtpTls": "starttls",
            "username": "alice",
            "password": "foobar",
            "email": "alice@localhost",
        }, jar="/tmp/login", expect=201)

        def inbox_has_seed(_):
            ids = decode_ids(api("GET", "/api/views/inbox/index", jar="/tmp/login"))
            if not ids:
                return False
            rows = json.loads(api("GET", "/api/threads?ids=" + ",".join(map(str, ids)), jar="/tmp/login"))
            return any(t["subject"] == "${subject}" for t in rows["threads"])

        retry(inbox_has_seed, timeout_seconds=300)

    machine.log(machine.succeed("systemd-analyze security tagmail.service | grep -v ✓"))
  '';
}
