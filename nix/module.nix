# NixOS module for shoald. Exposed as `nixosModules.shoald` by the flake.
#
# The configuration is written from `services.shoald.settings` and validated at build time
# with `shoald config check`, so a bad configuration fails `nixos-rebuild`, not the service.
#
# Identity: on first start the service creates a device key and logs the device ID
# (`journalctl -u shoald`). Certify that ID with `shoald certify` where the user key
# lives, then either set `certificateFile` or run, once,
#   sudo -u shoal shoald init --data-dir <dataDir> --name <name> --certificate <file>
{ defaultPackage }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.shoald;
  settingsFormat = pkgs.formats.toml { };
  configFile =
    pkgs.runCommand "shoald.toml"
      {
        nativeBuildInputs = [ cfg.package ];
      }
      ''
        cp ${settingsFormat.generate "shoald.toml" cfg.settings} $out
        shoald config check $out
      '';
  folderPaths = map (f: toString f.path) (cfg.settings.folders or [ ]);
in
{
  options.services.shoald = {
    enable = lib.mkEnableOption "the Shoal peer daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage pkgs;
      defaultText = lib.literalExpression "shoal.packages.\${system}.shoal";
      description = "Package providing `shoald`.";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/shoal";
      description = "State directory: device key, certificate, block store and metadata.";
    };

    deviceName = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName;
      defaultText = lib.literalExpression "config.networking.hostName";
      description = "Device name recorded when importing `certificateFile`.";
    };

    certificateFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        Device certificate issued by `shoald certify` for this device's ID. Imported before
        every start. Leave unset to manage the certificate by hand.
      '';
    };

    relayTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = ''
        File holding the token for self-hosted relays that admit only holders of it
        (`access.shared_token` in iroh-relay), with `settings.network.relay.mode = "custom"`.
        Given to the service as a systemd credential, so it may be readable by root alone, and
        kept out of the Nix store. Use a path to a secret, not a Nix path literal, which would
        copy it into the store.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Open the UDP port of `settings.network.bind` in the firewall.";
    };

    settings = lib.mkOption {
      inherit (settingsFormat) type;
      default = { };
      description = ''
        Contents of the shoald configuration file. Run `shoald config schema` for the
        JSON Schema of the version you deploy.
      '';
      example = lib.literalExpression ''
        {
          network.bind = "0.0.0.0:7777";
          folders = [{
            id = "<64 hex digits>";
            path = "/srv/shoal/photos";
            members = [ "<user id>" ];
            retention_days = 30;
          }];
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    services.shoald.settings.data_dir = lib.mkDefault (toString cfg.dataDir);
    services.shoald.settings.network.relay.token_file = lib.mkIf (
      cfg.relayTokenFile != null
    ) "/run/credentials/shoald.service/relay-token";

    users.users.shoal = {
      isSystemUser = true;
      group = "shoal";
      home = cfg.dataDir;
    };
    users.groups.shoal = { };

    environment.systemPackages = [ cfg.package ];

    networking.firewall.allowedUDPPorts =
      let
        bind = cfg.settings.network.bind or null;
        port = if bind == null then null else lib.toInt (lib.last (lib.splitString ":" bind));
      in
      lib.optional (cfg.openFirewall && port != null && port != 0) port;

    systemd.tmpfiles.rules = [ "d ${toString cfg.dataDir} 0700 shoal shoal -" ];

    systemd.services.shoald = {
      description = "Shoal peer daemon";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      restartTriggers = [ configFile ];
      serviceConfig = {
        ExecStartPre = [
          "${cfg.package}/bin/shoald id --data-dir ${toString cfg.dataDir}"
        ]
        ++ lib.optional (cfg.certificateFile != null) (
          "${cfg.package}/bin/shoald init --data-dir ${toString cfg.dataDir}"
          + " --name ${lib.escapeShellArg cfg.deviceName} --certificate ${toString cfg.certificateFile}"
        );
        ExecStart = "${cfg.package}/bin/shoald run --config ${configFile}";
        User = "shoal";
        Group = "shoal";
        Restart = "on-failure";
        RestartSec = 5;
        LoadCredential = lib.optional (
          cfg.relayTokenFile != null
        ) "relay-token:${toString cfg.relayTokenFile}";

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ReadWritePaths = [
          (toString cfg.dataDir)
        ]
        ++ folderPaths
        # Folder metadata kept apart from data_dir, such as on an SSD (ADR 0034).
        ++ lib.optional (cfg.settings ? metadata_dir) (toString cfg.settings.metadata_dir);
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectProc = "invisible";
        # No devices and no capabilities: the daemon needs neither. FUSE mounts (`mount` on a
        # folder) need both, so they are not available under this service.
        PrivateDevices = true;
        CapabilityBoundingSet = "";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RemoveIPC = true;
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
          "~@resources"
        ];
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_NETLINK"
          "AF_UNIX"
        ];
      };
    };
  };
}
