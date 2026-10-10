# NixOS module for tidefelld. Exposed as `nixosModules.tidefelld` by the flake.
#
# The configuration is written from `services.tidefelld.settings` and validated at build time
# with `tidefelld config check`, so a bad configuration fails `nixos-rebuild`, not the service.
#
# Identity: on first start the service creates a device key and logs the device ID
# (`journalctl -u tidefelld`). Certify that ID with `tidefelld certify` where the user key
# lives, then either set `certificateFile` or run, once,
#   sudo -u tidefell tidefelld init --data-dir <dataDir> --name <name> --certificate <file>
{ defaultPackage }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.tidefelld;
  settingsFormat = pkgs.formats.toml { };
  configFile =
    pkgs.runCommand "tidefelld.toml"
      {
        nativeBuildInputs = [ cfg.package ];
      }
      ''
        cp ${settingsFormat.generate "tidefelld.toml" cfg.settings} $out
        tidefelld config check $out
      '';
  folderPaths = map (f: toString f.path) (cfg.settings.folders or [ ]);
in
{
  options.services.tidefelld = {
    enable = lib.mkEnableOption "the Tidefell peer daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage pkgs;
      defaultText = lib.literalExpression "tidefell.packages.\${system}.tidefell";
      description = "Package providing `tidefelld`.";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/tidefell";
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
        Device certificate issued by `tidefelld certify` for this device's ID. Imported before
        every start. Leave unset to manage the certificate by hand.
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
        Contents of the tidefelld configuration file. Run `tidefelld config schema` for the
        JSON Schema of the version you deploy.
      '';
      example = lib.literalExpression ''
        {
          network.bind = "0.0.0.0:7777";
          folders = [{
            id = "<64 hex digits>";
            path = "/srv/tidefell/photos";
            members = [ "<user id>" ];
            retention_days = 30;
          }];
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    services.tidefelld.settings.data_dir = lib.mkDefault (toString cfg.dataDir);

    users.users.tidefell = {
      isSystemUser = true;
      group = "tidefell";
      home = cfg.dataDir;
    };
    users.groups.tidefell = { };

    environment.systemPackages = [ cfg.package ];

    networking.firewall.allowedUDPPorts =
      let
        bind = cfg.settings.network.bind or null;
        port = if bind == null then null else lib.toInt (lib.last (lib.splitString ":" bind));
      in
      lib.optional (cfg.openFirewall && port != null && port != 0) port;

    systemd.tmpfiles.rules = [ "d ${toString cfg.dataDir} 0700 tidefell tidefell -" ];

    systemd.services.tidefelld = {
      description = "Tidefell peer daemon";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      restartTriggers = [ configFile ];
      serviceConfig = {
        ExecStartPre = [
          "${cfg.package}/bin/tidefelld id --data-dir ${toString cfg.dataDir}"
        ]
        ++ lib.optional (cfg.certificateFile != null) (
          "${cfg.package}/bin/tidefelld init --data-dir ${toString cfg.dataDir}"
          + " --name ${lib.escapeShellArg cfg.deviceName} --certificate ${toString cfg.certificateFile}"
        );
        ExecStart = "${cfg.package}/bin/tidefelld run --config ${configFile}";
        User = "tidefell";
        Group = "tidefell";
        Restart = "on-failure";
        RestartSec = 5;

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ReadWritePaths = [ (toString cfg.dataDir) ] ++ folderPaths;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
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
