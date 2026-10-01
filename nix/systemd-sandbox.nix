# Systemd hardening for the NixOS ai-memory module. Packaged FHS units under
# `packaging/systemd/` keep their existing lighter hardening.
{ lib, ... }:

{
  # Full sandbox for the system unit (StateDirectory + /var/lib/ai-memory).
  aiMemorySystemSandbox = {
    CapabilityBoundingSet = [ ];
    AmbientCapabilities = [ ];
    MemoryDenyWriteExecute = true;
    RestrictAddressFamilies = [
      "AF_UNIX"
      "AF_INET"
      "AF_INET6"
    ];
    RestrictNamespaces = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    PrivateDevices = true;
    RemoveIPC = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    SystemCallArchitectures = "native";
    UMask = "0077";
    NoNewPrivileges = true;
    PrivateTmp = true;
    ProtectHome = true;
    ProtectSystem = "strict";
  };

}
