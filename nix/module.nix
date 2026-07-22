self:
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.oxidom;
  pkg = self.packages.${pkgs.system}.oxidom;
in
{
  options.programs.oxidom = {
    enable = lib.mkEnableOption "oxidom, a GTK4 Xray client";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkg;
      description = "The oxidom package to use.";
    };

    perAppRouting = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Install the privileged helper needed for `oxidom run -- <cmd>`
        (per-process routing via a network namespace). Placeholder until the
        helper's privilege model is finalized (setuid vs polkit vs systemd).
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    # NOTE: perAppRouting wiring (privileged helper) is intentionally not yet
    # implemented — see .notes/HANDOFF.md open questions.
  };
}
