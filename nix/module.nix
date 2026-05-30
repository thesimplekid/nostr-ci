{ self }:
{ config
, lib
, pkgs
, ...
}:
let
  cfg = config.services.runner-controller;
  package =
    if cfg.package == null then
      self.packages.${pkgs.stdenv.hostPlatform.system}.default
    else
      cfg.package;
  cdkWorkDir =
    if cfg.cdkWorkDir == null then
      "${cfg.stateDir}/cdk-cli"
    else
      cfg.cdkWorkDir;

  toCsv = lib.concatMapStringsSep ",";
  workerSoftware = toCsv
    (
      software: "${software.name}:${software.version}:${software.path}"
    )
    cfg.workerSoftware;
  workerPrices = toCsv
    (
      price: "${price.mintUrl}:${toString price.pricePerSecond}:${price.unit}"
    )
    cfg.workerPrices;

  baseEnvironment = {
    NOSTR_RELAYS = lib.concatStringsSep "," cfg.relays;
    WORKER_SOFTWARE = workerSoftware;
    WORKER_PRICES = workerPrices;
    WORKER_NAME = cfg.workerName;
    WORKER_DESCRIPTION = cfg.workerDescription;
    WORKER_ARCHITECTURE = cfg.workerArchitecture;
    WORKER_DEFAULT_SHELL = cfg.workerDefaultShell;
    WORKER_MIN_DURATION = toString cfg.workerMinDuration;
    WORKER_MAX_DURATION = toString cfg.workerMaxDuration;
    WORKER_MAX_CONCURRENT_JOBS = toString cfg.workerMaxConcurrentJobs;
    WORKER_SERVICE_NAME = cfg.workerServiceName;
    WORKER_ACT_PATH = cfg.workerActPath;
    WORKER_NGIT_PATH = cfg.workerNgitPath;
    WORKER_GIT_REMOTE_NOSTR_PATH = cfg.workerGitRemoteNostrPath;
    WORKER_WORK_DIR = cfg.workerWorkDir;
    WORKER_HTTP_PORT = toString cfg.workerHttpPort;
    MAX_CONCURRENT = toString cfg.maxConcurrent;
    POLL_INTERVAL = toString cfg.pollInterval;
    JOB_TIMEOUT = toString cfg.jobTimeout;
    ADVERTISE_INTERVAL = toString cfg.advertiseInterval;
    HTTP_PORT = toString cfg.httpPort;
    STATE_DIR = cfg.stateDir;
    CDK_CLI_PATH = cfg.cdkCliPath;
    CDK_WORK_DIR = cdkWorkDir;
    CDK_ENGINE = cfg.cdkEngine;
    NIXOS_CONTAINER_BIN = cfg.nixosContainerBin;
    RUST_LOG = cfg.logLevel;
  };
in
{
  options.services.runner-controller = {
    enable = lib.mkEnableOption "runner-controller";

    package = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "runner-controller package to run.";
    };

    relays = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "wss://relay.example" ];
      description = "Nostr relay URLs used for worker advertisements and job events.";
    };

    workerSoftware = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            name = lib.mkOption { type = lib.types.str; };
            version = lib.mkOption { type = lib.types.str; };
            path = lib.mkOption {
              type = lib.types.str;
              example = "/run/current-system/sw/bin/nix";
            };
          };
        }
      );
      default = [ ];
      description = "Software advertised by each worker.";
    };

    workerPrices = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            mintUrl = lib.mkOption {
              type = lib.types.str;
              example = "https://mint.example";
            };
            pricePerSecond = lib.mkOption {
              type = lib.types.ints.positive;
              example = 10;
            };
            unit = lib.mkOption {
              type = lib.types.str;
              default = "sat";
            };
          };
        }
      );
      default = [ ];
      description = "Cashu prices advertised by each worker.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/runner-controller";
      description = "Persistent controller state directory.";
    };

    cdkCliPath = lib.mkOption {
      type = lib.types.str;
      default = "/usr/local/bin/cdk-cli";
      description = "Path to a cdk-cli v0.16.0-compatible binary.";
    };

    cdkWorkDir = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/var/lib/runner-controller/cdk-cli";
      description = "cdk-cli wallet work directory. Defaults to stateDir/cdk-cli.";
    };

    cdkEngine = lib.mkOption {
      type = lib.types.enum [
        "redb"
        "sqlite"
      ];
      default = "redb";
      description = "cdk-cli wallet database engine.";
    };

    nixosContainerBin = lib.mkOption {
      type = lib.types.str;
      default = lib.getExe config.system.build.nixos-container;
      defaultText = lib.literalExpression "lib.getExe config.system.build.nixos-container";
      description = "Path to the nixos-container executable used to manage worker containers.";
    };

    workerName = lib.mkOption {
      type = lib.types.str;
      default = "loom-worker";
    };

    workerDescription = lib.mkOption {
      type = lib.types.str;
      default = "";
    };

    workerArchitecture = lib.mkOption {
      type = lib.types.str;
      default = pkgs.stdenv.hostPlatform.system;
    };

    workerDefaultShell = lib.mkOption {
      type = lib.types.str;
      default = "/bin/bash";
    };

    workerGeohash = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
    };

    workerMinDuration = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1;
      description = "Minimum billable job duration in seconds.";
    };

    workerMaxDuration = lib.mkOption {
      type = lib.types.ints.positive;
      default = 7200;
      description = "Maximum accepted job duration in seconds.";
    };

    workerMaxConcurrentJobs = lib.mkOption {
      type = lib.types.ints.positive;
      default = 1;
    };

    maxConcurrent = lib.mkOption {
      type = lib.types.ints.positive;
      default = 7;
      description = "Warm worker pool size.";
    };

    pollInterval = lib.mkOption {
      type = lib.types.ints.positive;
      default = 10;
      description = "Pool maintenance poll interval in seconds.";
    };

    jobTimeout = lib.mkOption {
      type = lib.types.ints.positive;
      default = 7200;
      description = "Fallback worker job timeout in seconds.";
    };

    advertiseInterval = lib.mkOption {
      type = lib.types.ints.positive;
      default = 300;
      description = "Worker advertisement interval in seconds.";
    };

    httpPort = lib.mkOption {
      type = lib.types.port;
      default = 8080;
    };

    workerHttpPort = lib.mkOption {
      type = lib.types.port;
      default = 8081;
    };

    workerServiceName = lib.mkOption {
      type = lib.types.str;
      default = "hive-worker.service";
      description = "systemd service name inside worker containers.";
    };

    workerActPath = lib.mkOption {
      type = lib.types.str;
      default = "act";
    };

    workerNgitPath = lib.mkOption {
      type = lib.types.str;
      default = "/usr/local/bin/ngit";
    };

    workerGitRemoteNostrPath = lib.mkOption {
      type = lib.types.str;
      default = "/usr/local/bin/git-remote-nostr";
    };

    workerWorkDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/loom-worker/work";
    };

    blossomServers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
    };

    cashuMints = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
    };

    containerTemplate = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      example = lib.literalExpression "./ci-container-template.nix";
      description = ''
        Optional NixOS container template installed as
        /etc/nixos/ci-container-template.nix. If unset, that file must already
        exist on the host.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the controller HTTP status port in the firewall.";
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
    };

    extraPackages = lib.mkOption {
      type = lib.types.listOf lib.types.package;
      default = [ ];
      description = "Extra packages added to the service PATH.";
    };

    environment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Additional environment variables for the service.";
    };

    serviceConfig = lib.mkOption {
      type = lib.types.attrs;
      default = { };
      description = "Extra systemd serviceConfig overrides.";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.relays != [ ];
        message = "services.runner-controller.relays must include at least one relay.";
      }
      {
        assertion = cfg.workerSoftware != [ ];
        message = "services.runner-controller.workerSoftware must include at least one entry.";
      }
      {
        assertion = cfg.workerPrices != [ ];
        message = "services.runner-controller.workerPrices must include at least one entry.";
      }
      {
        assertion = cfg.workerMinDuration <= cfg.workerMaxDuration;
        message = "services.runner-controller.workerMinDuration must be <= workerMaxDuration.";
      }
    ];

    environment.etc = lib.optionalAttrs (cfg.containerTemplate != null) {
      "nixos/ci-container-template.nix".source = cfg.containerTemplate;
    };

    networking.firewall.allowedTCPPorts = lib.optionals cfg.openFirewall [ cfg.httpPort ];

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0700 root root -"
      "d ${cdkWorkDir} 0700 root root -"
      "d ${cfg.workerWorkDir} 0755 root root -"
    ];

    systemd.services.runner-controller = {
      description = "Loom runner controller";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];

      path =
        with pkgs;
        [
          bash
          coreutils
          curl
          git
          iproute2
          systemd
        ]
        ++ cfg.extraPackages;

      environment =
        baseEnvironment
        // lib.optionalAttrs (cfg.workerGeohash != null) {
          WORKER_GEOHASH = cfg.workerGeohash;
        }
        // lib.optionalAttrs (cfg.blossomServers != [ ]) {
          BLOSSOM_SERVERS = lib.concatStringsSep "," cfg.blossomServers;
        }
        // lib.optionalAttrs (cfg.cashuMints != [ ]) {
          CASHU_MINTS = lib.concatStringsSep "," cfg.cashuMints;
        }
        // cfg.environment;

      serviceConfig = {
        Type = "simple";
        ExecStart = lib.getExe package;
        WorkingDirectory = cfg.stateDir;
        User = "root";
        Restart = "on-failure";
        RestartSec = "10s";
        KillSignal = "SIGTERM";
      } // cfg.serviceConfig;
    };
  };
}
