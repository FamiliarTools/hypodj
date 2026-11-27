# ONE shared module definition for both NixOS and Home-Manager.
#
# Called as `(import ./hypodj-module.nix) { flavor = "nixos" | "home-manager"; }`
# which yields an ordinary `{ config, lib, pkgs, ... }` module. Every option and
# the config.toml rendering are shared; `flavor` only selects the systemd schema
# (system service vs user service) and which hardening is emitted.
#
# SECRETS: the Navidrome password never enters the Nix store. The store holds
# only a template config.toml whose [server] table carries the literal
# placeholder `password = "@HYPODJ_PASSWORD@"`. A pre-start render step reads the
# real password from passwordFile / passwordCommand, TOML-escapes it, substitutes
# the placeholder, and writes the final config to a 0600 tmpfs RuntimeDirectory
# file. Plaintext only ever lives in tmpfs at runtime and in the (sops-managed)
# passwordFile outside the store.
{ flavor }:

{ config, lib, pkgs, ... }:

let
  inherit (lib) mkOption mkEnableOption mkIf types literalExpression;

  cfg = config.services.hypodj;

  isNixos = flavor == "nixos";

  tomlFormat = pkgs.formats.toml { };

  # The placeholder the render step replaces at runtime. Chosen so it round-trips
  # through pkgs.formats.toml as an ordinary basic-string value inside [server].
  passwordPlaceholder = "@HYPODJ_PASSWORD@";

  # Base (non-secret) attrs. `password` is present ONLY as the placeholder so the
  # generated template is a COMPLETE, parseable config for the daemon's required
  # `password` field - the runtime render just swaps the placeholder for the real
  # value in-place. url/username/client_name are non-secret and live here too.
  baseSettings = {
    server = {
      url = cfg.server.url;
      username = cfg.server.username;
      client_name = cfg.server.clientName;
      password = passwordPlaceholder;
    };
    mpd = {
      bind = cfg.mpd.bind;
    };
  };

  # Deep-merge the freeform `settings` escape hatch over the base. `settings`
  # cannot carry server.password (asserted below), so it can never leak plaintext
  # into the store-resident template.
  merged = lib.recursiveUpdate baseSettings cfg.settings;

  # The template lands in /nix/store. Safe: it contains only the placeholder, no
  # real password. Both flavors reference this same file.
  templateFile = tomlFormat.generate "hypodj-config-template.toml" merged;

  # The systemd credential name staged by LoadCredential (both flavors when a
  # passwordFile is set). systemd copies passwordFile into a per-service tmpfs
  # readable by the service uid at $CREDENTIALS_DIRECTORY/<name>.
  credentialName = "hypodj-password";

  # How the render step obtains the plaintext password on stdin of `pw=$(...)`.
  #  - passwordCommand: run it directly in ExecStartPre. NOTE: this runs with the
  #    SERVICE's privileges/sandbox (DynamicUser, ProtectHome=true on NixOS), so
  #    it must not need to read anything the hardened service cannot reach. Use
  #    passwordFile (LoadCredential) for sops secrets that live under the user's
  #    home or are root-owned.
  #  - passwordFile: read from the systemd credential directory. LoadCredential
  #    stages the file into a tmpfs the service uid can read, which works with
  #    DynamicUser + ProtectHome=true (NixOS) and with a user service (HM) whose
  #    manager supports credentials. This sidesteps the
  #    DynamicUser/ProtectHome-cannot-read-the-sops-secret trap entirely.
  #    Fallback (HM without credential support): read passwordFile directly - the
  #    user service runs as the file's owner so that is fine.
  useLoadCredential = cfg.server.passwordFile != null;

  passwordSource =
    if cfg.server.passwordCommand != null then
      lib.escapeShellArgs cfg.server.passwordCommand
    else
      # Prefer the LoadCredential-staged copy; on a user manager without
      # credential support, $CREDENTIALS_DIRECTORY is unset and we fall back to
      # reading the passwordFile path directly.
      ''cat "''${CREDENTIALS_DIRECTORY:-}/${credentialName}" 2>/dev/null || cat ${lib.escapeShellArg (toString cfg.server.passwordFile)}'';

  # Shared render script. Reads the password, TOML-escapes it (backslash then
  # double-quote - the only two metacharacters that break a TOML basic string;
  # a real newline in a password would still be invalid TOML and is rejected),
  # substitutes the placeholder, writes the final config under umask 077.
  #
  # RUNTIME_CONFIG is passed in by the unit (differs per flavor: /run/hypodj vs
  # %t/hypodj), never hardcoded here. The password is never echoed or logged.
  renderScript = pkgs.writeShellApplication {
    name = "hypodj-render-config";
    text = ''
      umask 077
      if [ -z "''${RUNTIME_CONFIG:-}" ]; then
        echo "hypodj-render-config: RUNTIME_CONFIG not set" >&2
        exit 1
      fi

      pw="$(${passwordSource})"

      case "$pw" in
        *$'\n'*)
          echo "hypodj-render-config: password contains a newline; not TOML-safe" >&2
          exit 1
          ;;
      esac

      # TOML basic-string escaping: backslash FIRST, then double-quote. These are
      # the only two metacharacters that break a TOML basic string (newline is
      # rejected above).
      esc="$pw"
      esc="''${esc//\\/\\\\}"
      esc="''${esc//\"/\\\"}"

      # Splice the escaped password in by literal prefix/suffix concatenation.
      # NOT sed (would re-interpret \" and &) and NOT ''${var//pat/repl} (bash 5.2+
      # treats a literal & in the replacement as the matched text). The placeholder
      # occurs exactly once, so prefix = everything before it, suffix = everything
      # after; printf glues prefix + esc + suffix with zero metacharacter magic.
      template="$(cat ${templateFile})"
      prefix="''${template%%${passwordPlaceholder}*}"
      suffix="''${template#*${passwordPlaceholder}}"
      if [ "$prefix" = "$template" ]; then
        echo "hypodj-render-config: placeholder ${passwordPlaceholder} not found in template" >&2
        exit 1
      fi
      printf '%s%s%s\n' "$prefix" "$esc" "$suffix" > "$RUNTIME_CONFIG"
    '';
  };

  # Field VALUES shared by both flavors. Each flavor assembles these into its OWN
  # systemd schema (NixOS lowercase serviceConfig/wantedBy; HM capital-case
  # Service/Unit/Install) - the two attrpath shapes are NOT interchangeable.
  execStartPre = "${renderScript}/bin/hypodj-render-config";
  execStart = ''${lib.getExe cfg.package} "$RUNTIME_CONFIG"'';
  # HYPODJ_AUDIO: "null" (default) keeps playback headless (ao=null); "device"
  # opens the real device. Default null so the service never grabs the speakers
  # while mopidy runs.
  audioEnv = "HYPODJ_AUDIO=${cfg.audio}";

  # ---- NixOS systemd.services.hypodj ----
  nixosService = {
    description = "hypodj MPD-to-OpenSubsonic daemon";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];

    # RUNTIME_CONFIG resolves to the RuntimeDirectory (tmpfs, 0700, wiped on stop).
    environment = {
      RUNTIME_CONFIG = "/run/hypodj/config.toml";
    };

    serviceConfig = {
      # Foreground long-running process; no sd_notify -> Type=simple (default).
      Environment = [ audioEnv ];
      ExecStartPre = execStartPre;
      ExecStart = "${pkgs.runtimeShell} -c ${lib.escapeShellArg execStart}";

      RuntimeDirectory = "hypodj";
      RuntimeDirectoryMode = "0700";

      # SMOOTH-RESTART: a PERSISTENT state dir for the resume snapshot
      # (resume.toml). systemd sets $STATE_DIRECTORY to this and the daemon reads
      # it. With DynamicUser=true systemd backs it at /var/lib/private/hypodj and
      # exposes it as /var/lib/hypodj; ProtectSystem=strict already permits
      # StateDirectory writes. This is NOT the RuntimeDirectory (/run tmpfs is
      # wiped on stop, which would defeat SIGKILL resume).
      StateDirectory = "hypodj";
      StateDirectoryMode = "0700";

      # Give the graceful shutdown (persist -> sleep-fade-out -> exit 0) time to
      # run before SIGKILL: TimeoutStopSec must exceed the daemon's internal fade
      # budget (~10s) with margin. KillSignal is SIGTERM (explicit), which the
      # daemon traps to fade out.
      TimeoutStopSec = 15;
      KillSignal = "SIGTERM";

      Restart = "on-failure";
      RestartSec = 5;

      # LoadCredential makes passwordFile readable by the (Dynamic)User via a
      # per-service tmpfs, removing the ownership race with root-owned sops
      # secrets. Only used when reading from a file (passwordCommand renders on
      # its own).
      LoadCredential =
        lib.optional useLoadCredential
          "${credentialName}:${toString cfg.server.passwordFile}";

      # Hardening (NixOS-system only). DynamicUser + LoadCredential is the
      # idiomatic secrets-safe combination: no fixed user needed, and the sops
      # secret's ownership no longer matters.
      DynamicUser = true;
      ProtectSystem = "strict";
      ProtectHome = true;
      PrivateTmp = true;
      NoNewPrivileges = true;
      # Loopback bind (6600) + outbound HTTPS to the OpenSubsonic server.
      RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
      # NOT set: MemoryDenyWriteExecute (would break libmpv codec paths).
    };
  };

  # ---- Home-Manager systemd.user.services.hypodj ----
  # RAW unit-file schema: capital-case Unit / Service / Install.
  hmService = {
    Unit = {
      Description = "hypodj MPD-to-OpenSubsonic daemon";
      # No network-online.target: it is not reliably satisfiable in the user
      # manager and can wedge the unit. A cold network no longer needs covering
      # here either: the daemon's startup greet WARNS instead of exiting, so it
      # binds MPD and restores its queue with the music server unreachable.
      # Restart=on-failure below therefore covers real crashes only - it is no
      # longer doubling as a network wait (it never was a good one: the old
      # `ping().await?` exit meant a sleeping server crash-looped the unit every
      # RestartSec without ever serving a client).
    };
    Service = {
      # Type=simple (default): foreground process, no sd_notify.
      Environment = [ audioEnv "RUNTIME_CONFIG=%t/hypodj/config.toml" ];
      ExecStartPre = execStartPre;
      ExecStart = "${pkgs.runtimeShell} -c ${lib.escapeShellArg execStart}";
      RuntimeDirectory = "hypodj";
      RuntimeDirectoryMode = "0700";
      # SMOOTH-RESTART: persistent state dir for the resume snapshot. The user
      # manager sets $STATE_DIRECTORY to %S/hypodj (~/.local/state/hypodj); the
      # daemon reads it. NOT the RuntimeDirectory (tmpfs, wiped on stop).
      StateDirectory = "hypodj";
      # Let the graceful shutdown (persist -> sleep-fade-out -> exit 0) finish
      # before SIGKILL; must exceed the internal fade budget (~10s) with margin.
      TimeoutStopSec = 15;
      KillSignal = "SIGTERM";
      Restart = "on-failure";
      RestartSec = 5;
      # LoadCredential is honored by recent user systemd; it stages passwordFile
      # into $CREDENTIALS_DIRECTORY the render step reads. On an older user
      # manager without credential support the variable is simply unset and the
      # render step falls back to reading passwordFile directly (the user service
      # runs as the file's owner, so that is safe).
    } // lib.optionalAttrs useLoadCredential {
      LoadCredential = "${credentialName}:${toString cfg.server.passwordFile}";
    } // {
      # NO DynamicUser/ProtectSystem/RestrictAddressFamilies here: those are
      # system-service-only and break user-service activation.
    };
    Install = {
      WantedBy = [ "default.target" ];
    };
  };

  # ---- the offline feature batch (services.hypodj.features) ----
  # hypodj-mir over the offline mirror into the daemon's features dir, as a
  # low-priority oneshot on a timer. It only READS the store and only WRITES the
  # features dir, which the daemon re-reads when its mtime moves.
  fcfg = cfg.features;
  # Same resolution as the daemon: `<state_dir>/store` and `<state_dir>/features`
  # unless the config overrides them.
  stateDir = if isNixos then "/var/lib/hypodj" else "%S/hypodj";
  storeDir =
    if fcfg.storeDir != null then fcfg.storeDir
    else cfg.settings.store.dir or "${stateDir}/store";
  featuresDir =
    if fcfg.outDir != null then fcfg.outDir
    else cfg.settings.features.dir or "${stateDir}/features";
  mirArgs =
    [ "${cfg.package}/bin/hypodj-mir" "--store" storeDir "--out" featuresDir
      "--jobs" (toString fcfg.jobs) ]
    ++ lib.optionals fcfg.models [
      "--models" "${fcfg.modelsPackage}"
      "--tf-python" "${fcfg.tfPython}/bin/python3"
      "--tf-script" "${cfg.package}/share/hypodj/mir_tf.py"
    ];
  # A unit's ExecStart is split by systemd, not a shell; %S must stay unquoted
  # for the specifier to expand, and none of these paths carry spaces.
  mirExec = lib.concatStringsSep " " mirArgs;
  # Resource bounds shared by both flavors (field VALUES, assembled per schema).
  mirResources = {
    Type = "oneshot";
    ExecStart = mirExec;
    Nice = 19;
    IOSchedulingClass = "idle";
    CPUSchedulingPolicy = "idle";
    CPUQuota = fcfg.cpuQuota;
    MemoryHigh = fcfg.memoryHigh;
    MemoryMax = fcfg.memoryMax;
    # The TF helper sizes its thread pool from the affinity mask; hypodj-mir
    # narrows it per worker, this caps the whole tree.
    Environment = [ "HYPODJ_AUDIO=null" ];
  };
  mirTimer = {
    OnCalendar = fcfg.schedule;
    # A laptop is often asleep at the scheduled time: catch up on wake.
    Persistent = true;
    RandomizedDelaySec = "15min";
  };
  mirDescription = "hypodj-mir: measure audio features of the offline mirror";

in
{
  options.services.hypodj = {
    enable = mkEnableOption "hypodj MPD-to-OpenSubsonic daemon";

    package = mkOption {
      type = types.package;
      default = pkgs.hypodj;
      defaultText = literalExpression "pkgs.hypodj";
      description = ''
        The hypodj package to run. Defaults to `pkgs.hypodj`, provided by this
        flake's overlay (imported automatically by the NixOS module). On
        Home-Manager, either add `hypodj.overlays.default` to nixpkgs.overlays or
        set this explicitly to `hypodj.packages.<system>.default`.
      '';
    };

    server.url = mkOption {
      type = types.str;
      example = "https://navidrome.example.com";
      description = "Base URL of the OpenSubsonic/Navidrome server. Required, non-secret.";
    };

    server.username = mkOption {
      type = types.str;
      example = "guilherme";
      description = "Username for the OpenSubsonic server. Required, non-secret.";
    };

    server.passwordFile = mkOption {
      # types.str (NOT types.path): a path is used verbatim as a RUNTIME path.
      # types.path would coerce a literal path into the Nix store (copying the
      # secret in). A sops runtime path like
      # config.sops.secrets."hypodj/password".path is already a string and must
      # stay a string so it is never copied to the store.
      type = types.nullOr types.str;
      default = null;
      example = literalExpression ''config.sops.secrets."hypodj/password".path'';
      description = ''
        Path (as a string) to a file containing the Navidrome password, read at
        service start into a runtime-only config. sops-nix friendly. The password
        never enters the Nix store - pass a runtime path string (e.g.
        `config.sops.secrets."hypodj/password".path`), not a literal store path.
        Exactly one of passwordFile / passwordCommand must be set.
      '';
    };

    server.passwordCommand = mkOption {
      type = types.nullOr (types.listOf types.str);
      default = null;
      example = literalExpression ''[ "cat" "/run/secrets/hypodj" ]'';
      description = ''
        Alternative to passwordFile: a command whose stdout is the password, run
        at service start. Exactly one of passwordFile / passwordCommand must be
        set.
      '';
    };

    server.clientName = mkOption {
      type = types.str;
      default = "hypodj";
      description = "Client name reported to the server (OpenSubsonic `c` param).";
    };

    mpd.bind = mkOption {
      type = types.str;
      default = "127.0.0.1:6600";
      description = ''
        Address the MPD-protocol listener binds to. Default 127.0.0.1:6600, the
        standard MPD port.
      '';
    };

    audio = mkOption {
      type = types.enum [ "null" "device" ];
      default = "null";
      description = ''
        Audio output policy (maps to HYPODJ_AUDIO). "null" (default) keeps hypodj
        headless so it never grabs the speakers; "device" opens the real device.
      '';
    };

    searchProvider = {
      enable = mkEnableOption ''
        the GNOME Shell search provider (type "next song", "louder" or an album
        name in Activities). Independent of `enable`: it is a client of whatever
        daemon HYPODJ_HOST/HYPODJ_PORT point at (default 127.0.0.1:6600), and is
        started on demand by D-Bus activation, so no service unit is needed
      '';
      package = mkOption {
        type = types.package;
        default = pkgs.hypodj-clients;
        defaultText = literalExpression "pkgs.hypodj-clients";
        description = ''
          The package carrying `dj-search-provider` and its registration files
          (share/gnome-shell/search-providers, share/applications,
          share/dbus-1/services).
        '';
      };
    };

    features = {
      enable = mkEnableOption ''
        the offline audio-feature batch: `hypodj-mir` over the offline mirror into
        the daemon's features dir, on a timer, at idle priority, only on AC power.
        It never runs the daemon or touches audio; it only reads the mirror
        (docs/mir.md)
      '';
      schedule = mkOption {
        type = types.str;
        default = "*-*-* 03:30:00";
        example = "hourly";
        description = "systemd OnCalendar for the batch timer (Persistent, so a missed run catches up on wake).";
      };
      models = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Also run the learned heads (voice/instrumental, relaxed, danceability,
          DEAM arousal/valence, the discogs-effnet embedding). Pulls
          `modelsPackage` (MTG models, CC BY-NC-SA 4.0: personal, non-commercial
          use only) and `tfPython` (the upstream essentia-tensorflow wheel,
          about 290 MB). Off = the classic extractor only (bpm, key, beats,
          loudness), no model, no TensorFlow.
        '';
      };
      modelsPackage = mkOption {
        type = types.package;
        # callPackage'd here rather than read from the overlay, so the default
        # resolves on Home-Manager too (where the overlay is not wired).
        default = pkgs.callPackage ./mir-models.nix { };
        defaultText = literalExpression "hypodj.packages.<system>.mir-models";
        description = "The pinned MTG model files (the flake's `.#mir-models`).";
      };
      tfPython = mkOption {
        type = types.package;
        default = pkgs.callPackage ./essentia-tensorflow.nix { };
        defaultText = literalExpression "hypodj.packages.<system>.mir-tf-python";
        description = "A Python env with `essentia-tensorflow` (the flake's `.#mir-tf-python`).";
      };
      jobs = mkOption {
        type = types.ints.between 1 4;
        default = 2;
        description = ''
          Worker ceiling. hypodj-mir admits each worker by measured headroom
          (free cores, MemAvailable) before every track, so this is only the cap.
        '';
      };
      cpuQuota = mkOption {
        type = types.str;
        default = "200%";
        description = "systemd CPUQuota for the whole batch tree (200% = two cores).";
      };
      memoryHigh = mkOption {
        type = types.str;
        default = "3G";
        description = "systemd MemoryHigh (reclaim pressure starts here).";
      };
      memoryMax = mkOption {
        type = types.str;
        default = "4G";
        description = "systemd MemoryMax (hard cap; a TF worker peaks at about 1.4 to 2.1 GiB).";
      };
      storeDir = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "The offline mirror to read. Default: the daemon's `<state_dir>/store` (or `settings.store.dir`).";
      };
      outDir = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Where to write the feature files. Default: the daemon's `<state_dir>/features` (or `settings.features.dir`).";
      };
    };

    settings = mkOption {
      type = types.attrsOf types.anything;
      default = { };
      description = ''
        Freeform escape hatch merged into the generated config.toml. Must NOT set
        server.password (that is injected at runtime from passwordFile /
        passwordCommand; setting it here is rejected to prevent leaking plaintext
        into the Nix store).
      '';
    };
  };

  config = lib.mkMerge [ (mkIf cfg.enable (
    {
      assertions = [
        {
          assertion =
            (cfg.server.passwordFile != null) != (cfg.server.passwordCommand != null);
          message =
            "services.hypodj: set exactly one of server.passwordFile or server.passwordCommand.";
        }
        {
          assertion = !(cfg.settings ? server && cfg.settings.server ? password);
          message =
            "services.hypodj.settings must not set server.password; the password is injected at runtime from passwordFile/passwordCommand and must never enter the Nix store.";
        }
        {
          assertion = cfg.server.url != "";
          message = "services.hypodj.server.url must be set.";
        }
        {
          assertion = cfg.server.username != "";
          message = "services.hypodj.server.username must be set.";
        }
      ];
    }
    # NixOS-only: wire the flake overlay so `pkgs.hypodj` resolves, and emit the
    # system service.
    // lib.optionalAttrs isNixos {
      nixpkgs.overlays = [ (import ./overlay.nix) ];
      systemd.services.hypodj = nixosService;
    }
    # Home-Manager-only: emit the user service.
    // lib.optionalAttrs (!isNixos) {
      systemd.user.services.hypodj = hmService;
    }
  ))
  # The offline feature batch: a oneshot service and its timer.
  (mkIf (cfg.enable && fcfg.enable) (
    lib.optionalAttrs isNixos {
      systemd.services.hypodj-mir = {
        description = mirDescription;
        # Only on AC: a missed run waits for the next tick (or Persistent catch-up).
        unitConfig.ConditionACPower = true;
        serviceConfig = mirResources // {
          # The same dynamic user as the daemon, so the state dir is shared.
          DynamicUser = true;
          User = "hypodj";
          StateDirectory = "hypodj";
          StateDirectoryMode = "0700";
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
          PrivateNetwork = true;
          NoNewPrivileges = true;
        };
      };
      systemd.timers.hypodj-mir = {
        wantedBy = [ "timers.target" ];
        timerConfig = mirTimer;
      };
    }
    // lib.optionalAttrs (!isNixos) {
      systemd.user.services.hypodj-mir = {
        Unit = {
          Description = mirDescription;
          ConditionACPower = true;
        };
        Service = mirResources // {
          # %S/hypodj exists once the daemon ran; the batch creates `features/`.
          StateDirectory = "hypodj";
        };
      };
      systemd.user.timers.hypodj-mir = {
        Unit.Description = "Timer for hypodj-mir";
        Timer = mirTimer;
        Install.WantedBy = [ "timers.target" ];
      };
    }
  ))
  # The GNOME search provider: the Shell finds the provider .ini and the .desktop
  # it names under XDG data dirs, and the session bus starts the service from its
  # activation file on the first search.
  (mkIf cfg.searchProvider.enable (
    let
      sp = cfg.searchProvider.package;
      busName = "tech.familiartools.HypoDJ.SearchProvider";
    in
    lib.optionalAttrs isNixos {
      nixpkgs.overlays = mkIf (!cfg.enable) [ (import ./overlay.nix) ];
      environment.systemPackages = [ sp ];
      environment.pathsToLink = [ "/share/gnome-shell/search-providers" ];
      services.dbus.packages = [ sp ];
    }
    // lib.optionalAttrs (!isNixos) {
      home.packages = [ sp ];
      # Linked into ~/.local/share as well, which the Shell and the session bus
      # always scan, rather than relying on the profile being on XDG_DATA_DIRS.
      xdg.dataFile."gnome-shell/search-providers/${busName}.ini".source =
        "${sp}/share/gnome-shell/search-providers/${busName}.ini";
      xdg.dataFile."dbus-1/services/${busName}.service".source =
        "${sp}/share/dbus-1/services/${busName}.service";
    }
  ))
  ];
}
