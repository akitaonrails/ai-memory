# NixOS module for the ai-memory MCP server.
#
# Standalone: no reference to `self` or anything flake-specific, so it stays
# importable outside this flake. `flake.nix`'s `nixosModules.default` wraps
# this file and supplies a default for `package` by closing over `self`.
{ config, lib, pkgs, utils, ... }:

let
  cfg = config.services.ai-memory;
  sandbox = import ./systemd-sandbox.nix { inherit lib; };

  defaultDataDir = "/var/lib/ai-memory";
  usesDefaultDataDir = cfg.dataDir == defaultDataDir;

  isLoopback = lib.elem cfg.bind [ "127.0.0.1" "::1" ];
  bindHost = if lib.hasInfix ":" cfg.bind then "[${cfg.bind}]" else cfg.bind;

  tomlFormat = pkgs.formats.toml { };

  # Strip null leaves so optional typed settings omit keys from generated TOML.
  pruneNulls =
    value:
    if value == null then
      null
    else if builtins.isAttrs value then
      lib.filterAttrs (_: v: v != null) (lib.mapAttrs (_: v: pruneNulls v) value)
    else
      value;

  settingsToml =
    let
      # Service-level bind/port/enableWeb win over duplicate settings keys.
      stripped = lib.filterAttrs (name: _: name != "bind") (pruneNulls cfg.settings);
    in
    if stripped == { } then
      null
    else
      tomlFormat.generate "ai-memory-config.toml" stripped;

  ageSecretPath =
    if cfg.ageSecret == null then
      null
    else
      config.age.secrets.${cfg.ageSecret}.path;

  sopsSecretPath =
    if cfg.sopsSecret == null then
      null
    else
      config.sops.secrets.${cfg.sopsSecret}.path;

  secretsFile =
    if cfg.ageSecret != null then
      ageSecretPath
    else if cfg.sopsSecret != null then
      sopsSecretPath
    else
      cfg.environmentFile;

  secretSourceCount = lib.count (source: source != null) [
    cfg.ageSecret
    cfg.sopsSecret
    cfg.environmentFile
  ];

  secretKeysInSettings =
    let
      auth = cfg.settings.auth or null;
    in
    lib.filter (k: auth != null && (auth.${k} or null) != null) [
      "bearer_token"
      "token_pepper"
      "initial_root_password"
      "recovery_token"
      "actor_proxy_bearer_token"
    ];

  execStartArgs =
    [
      (lib.getExe cfg.package)
      "--data-dir"
      cfg.dataDir
    ]
    ++ lib.optionals (settingsToml != null) [
      "--config"
      "${settingsToml}"
    ]
    ++ [
      "serve"
      "--transport"
      "http"
      "--bind"
      "${bindHost}:${toString cfg.port}"
    ]
    ++ lib.optionals cfg.enableApi [ "--enable-api" ]
    ++ lib.optionals cfg.enableWeb [ "--enable-web" ];
in
{
  options.services.ai-memory = {
    enable = lib.mkEnableOption "the ai-memory MCP server as a systemd service";

    package = lib.mkOption {
      type = lib.types.package;
      description = ''
        The ai-memory package to run. No default here, so this module stays
        usable standalone. This repo's `flake.nix` supplies a default via
        `nixosModules.default`, which sets
        `services.ai-memory.package = lib.mkDefault
        self.packages.${pkgs.stdenv.hostPlatform.system}.default`.
        A consumer importing this file directly (bypassing that wrapper)
        must set this option themselves.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = defaultDataDir;
      description = "Data directory passed as --data-dir (wiki, SQLite, config.toml).";
    };

    bind = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1";
      description = "Host ai-memory's HTTP transport binds to.";
    };

    port = lib.mkOption {
      type = lib.types.port;
      default = 49374;
      description = "Port ai-memory's HTTP transport binds to.";
    };

    enableWeb = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Pass --enable-web (mounts the built-in web UI at /web). Off by
        default, matching the underlying --enable-web CLI flag's own
        default — NOT the packaged FHS systemd unit, which hardcodes it on.
        Enabling the web UI also mounts `/api/v1`; `enableApi` is unnecessary
        in that mode.
      '';
    };

    enableApi = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Pass --enable-api to mount the protected read-only `/api/v1` surface
        without the browser UI. Off by default.
      '';
    };

    settings = lib.mkOption {
      type = lib.types.submodule {
        freeformType = tomlFormat.type;
        options = import ./settings-options.nix { inherit lib; };
      };
      default = { };
      description = ''
        Declarative config.toml fragment (non-secret keys only). Rendered to a
        generated file and passed as `--config`. Top-level `bind`, `port`, and
        `enableWeb`, and `enableApi` service options win over duplicate keys
        here.

        A small typed set covers common keys; `freeformType` accepts any other
        TOML key. Values here land in a world-readable Nix store path — never
        put secrets in `settings` (including `llm_headers` / auth tokens);
        use `ageSecret`, `sopsSecret`, or `environmentFile` instead.
      '';
    };

    ageSecret = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "ai-memory-env";
      description = ''
        Name of a `config.age.secrets.<name>` entry (agenix). Mutually
        exclusive with `sopsSecret` and `environmentFile`. The host must import
        agenix; this module only consumes the decrypted path.
      '';
    };

    sopsSecret = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "ai-memory/env";
      description = ''
        Name of a `config.sops.secrets.<name>` entry (sops-nix). Mutually
        exclusive with `ageSecret` and `environmentFile`. The host must import
        sops-nix; this module only consumes the decrypted path.
      '';
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Escape hatch: explicit systemd EnvironmentFile path for secrets such as
        `AI_MEMORY_AUTH_TOKEN` (required once `bind` is non-loopback). On
        loopback, a missing file does not fail service start (leading `-`).
        On non-loopback the file must exist. Mutually exclusive with
        `ageSecret` and `sopsSecret`; prefer those when using agenix or
        sops-nix.
      '';
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "ai-memory";
      description = "System user the service runs as.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "ai-memory";
      description = "System group the service runs as.";
    };

    createUser = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Create the dedicated system user and group. Defaults to true when
        `user` is `ai-memory`; set false when `user` names an existing
        account you manage elsewhere.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the service `port` in the firewall when true.";
    };

    memoryMax = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "2G";
      description = "Optional systemd MemoryMax for the service.";
    };

    tasksMax = lib.mkOption {
      type = lib.types.nullOr lib.types.int;
      default = null;
      description = "Optional systemd TasksMax for the service.";
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        assertions = [
          {
            assertion = secretSourceCount <= 1;
            message = "services.ai-memory: ageSecret, sopsSecret, and environmentFile are mutually exclusive";
          }
          {
            assertion = isLoopback || secretsFile != null;
            message =
              "services.ai-memory: non-loopback bind requires ageSecret, sopsSecret, or environmentFile (for AI_MEMORY_AUTH_TOKEN and related secrets)";
          }
          {
            assertion = secretKeysInSettings == [ ];
            message =
              "services.ai-memory.settings.auth must not contain secrets (${lib.concatStringsSep ", " secretKeysInSettings}); use ageSecret/sopsSecret/environmentFile";
          }
          {
            assertion = (cfg.settings.llm_headers or null) == null;
            message =
              "services.ai-memory.settings.llm_headers may contain credentials and must not enter the Nix store; use AI_MEMORY_LLM_HEADERS in ageSecret/sopsSecret/environmentFile";
          }
          {
            assertion = (cfg.settings.bind or null) == null;
            message = "services.ai-memory.settings.bind is not allowed; use the top-level bind/port options";
          }
        ];

        services.ai-memory.createUser = lib.mkDefault (cfg.user == "ai-memory");

        environment.systemPackages = [ cfg.package ];

        networking.firewall.allowedTCPPorts = lib.optionals cfg.openFirewall [ cfg.port ];
      }

      (lib.mkIf cfg.createUser {
        users.users.${cfg.user} = {
          isSystemUser = true;
          group = cfg.group;
          description = "ai-memory MCP server";
          home = cfg.dataDir;
          createHome = false;
          shell = "${pkgs.util-linuxMinimal}/bin/nologin";
        };
        users.groups.${cfg.group} = { };
      })

      (lib.mkIf (!usesDefaultDataDir && cfg.createUser) {
        systemd.tmpfiles.settings."10-ai-memory"."${cfg.dataDir}".d = {
          mode = "0750";
          user = cfg.user;
          group = cfg.group;
        };
      })

      {
        systemd.services.ai-memory = {
          description = "ai-memory MCP server";
          documentation = [ "https://github.com/akitaonrails/ai-memory" ];
          after = if isLoopback then [ "network.target" ] else [ "network-online.target" ];
          wants = lib.optionals (!isLoopback) [ "network-online.target" ];
          wantedBy = [ "multi-user.target" ];

          serviceConfig = lib.mkMerge [
            {
              Type = "simple";
              User = cfg.user;
              Group = cfg.group;
              ExecStart = utils.escapeSystemdExecArgs execStartArgs;
              Restart = "on-failure";
              RestartSec = "5s";
              TimeoutStopSec = "30s";
              StartLimitBurst = 5;
              StartLimitIntervalSec = "60s";
              ReadWritePaths = [ cfg.dataDir ];
            }
            (lib.optionalAttrs usesDefaultDataDir {
              StateDirectory = "ai-memory";
              StateDirectoryMode = "0750";
            })
            sandbox.aiMemorySystemSandbox
            (if secretsFile != null then
              if isLoopback then
                { EnvironmentFile = "-${secretsFile}"; }
              else
                { EnvironmentFile = "${secretsFile}"; }
            else
              { })
            (lib.optionalAttrs (cfg.memoryMax != null) {
              MemoryMax = cfg.memoryMax;
            })
            (lib.optionalAttrs (cfg.tasksMax != null) {
              TasksMax = cfg.tasksMax;
            })
          ];
        };
      }
    ]
  );
}
