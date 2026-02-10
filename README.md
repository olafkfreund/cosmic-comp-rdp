# cosmic-comp-rdp

Compositor for the [COSMIC Desktop Environment](https://github.com/pop-os/cosmic-epoch) with RemoteDesktop EIS (Emulated Input Server) support.

This is a fork of [pop-os/cosmic-comp](https://github.com/pop-os/cosmic-comp) that adds the ability to receive input events from remote desktop sessions. The EIS receiver allows the [xdg-desktop-portal-cosmic](https://github.com/olafkfreund/xdg-desktop-portal-cosmic) RemoteDesktop portal to inject keyboard and mouse input into the compositor on behalf of RDP clients.

## Features

- **Full COSMIC compositor functionality** (window management, workspaces, tiling, animations, etc.)
- **EIS input receiver** via D-Bus (`com.system76.CosmicComp.RemoteDesktop`)
- **Keyboard injection** from remote sessions (full scancode support)
- **Pointer injection** (relative motion, absolute position, buttons, scroll)
- **Per-session isolation** via UNIX socket pairs managed by the portal
- **Calloop integration** for non-blocking event processing in the compositor event loop

## EIS Input Receiver

The compositor exposes a D-Bus interface for accepting EIS socket file descriptors from the portal:

```
Bus Name:  com.system76.CosmicComp.RemoteDesktop
Object:    /com/system76/CosmicComp
Interface: com.system76.CosmicComp.RemoteDesktop
Method:    AcceptEisSocket(fd: OwnedFd)
```

### How it works

1. The xdg-desktop-portal-cosmic RemoteDesktop portal creates a UNIX socket pair during `Start`
2. The server-side fd is sent to the compositor via `AcceptEisSocket`
3. The compositor performs the EIS handshake (server side) and creates a seat with keyboard + pointer capabilities
4. Input events from the remote client are injected into Smithay's input pipeline
5. Injected events are indistinguishable from local hardware input

### Input events supported

| Event | Description |
|-------|-------------|
| `KeyboardKey` | Key press/release with evdev keycodes |
| `PointerMotion` | Relative mouse movement (dx, dy) |
| `PointerMotionAbsolute` | Absolute mouse position (x, y) |
| `Button` | Mouse button press/release (left, right, middle, etc.) |
| `ScrollDelta` | Smooth scroll (dx, dy) |
| `ScrollDiscrete` | Discrete scroll steps |

## Building

### Using Nix (recommended)

```bash
nix develop              # Enter dev shell with all dependencies
cargo build --release    # Build release binary

# Or build directly with Nix
nix build
```

### Using Cargo (requires system libraries)

Ensure the following development headers are installed: Wayland, libxkbcommon, libinput, Mesa, seatd, libei, systemd, fontconfig, pixman, libdisplay-info.

```bash
cargo build --release
```

### Rust version

Requires Rust 1.90+ (edition 2024). The `rust-toolchain.toml` file specifies the exact version.

## NixOS Module

The flake provides a NixOS module for declarative configuration.

### Basic setup

```nix
{
  inputs.cosmic-comp.url = "github:olafkfreund/cosmic-comp-rdp";

  outputs = { self, nixpkgs, cosmic-comp, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        cosmic-comp.nixosModules.default
        {
          nixpkgs.overlays = [ cosmic-comp.overlays.default ];

          services.cosmic-comp = {
            enable = true;
            eis.enable = true;  # enabled by default

            settings = {
              xkb-config = {
                layout = "us";
              };
            };
          };
        }
      ];
    };
  };
}
```

### Module options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Enable the COSMIC compositor |
| `package` | package | `pkgs.cosmic-comp` | Compositor package to use |
| `eis.enable` | bool | `true` | Enable the EIS D-Bus interface for remote input |
| `settings` | attrs | `{}` | Compositor configuration (freeform TOML) |
| `settings.xkb-config.layout` | string | `"us"` | XKB keyboard layout |
| `settings.xkb-config.variant` | string | `""` | XKB layout variant |
| `settings.xkb-config.options` | string | `""` | XKB options (e.g., `ctrl:nocaps`) |
| `settings.xkb-config.model` | string | `""` | XKB keyboard model |

The module automatically enables:
- `hardware.graphics` (GPU acceleration)
- `services.seatd` (seat management)
- `security.polkit` (device access)
- D-Bus registration for EIS (when `eis.enable = true`)

## Home Manager Module

For user-level configuration of the compositor.

```nix
{
  inputs.cosmic-comp.url = "github:olafkfreund/cosmic-comp-rdp";

  outputs = { self, nixpkgs, home-manager, cosmic-comp, ... }: {
    homeConfigurations."user" = home-manager.lib.homeManagerConfiguration {
      modules = [
        cosmic-comp.homeManagerModules.default
        {
          nixpkgs.overlays = [ cosmic-comp.overlays.default ];

          wayland.compositor.cosmic-comp = {
            enable = true;

            xkb = {
              layout = "de";
              variant = "nodeadkeys";
              options = "ctrl:nocaps";
            };
          };
        }
      ];
    };
  };
}
```

### Home Manager options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `enable` | bool | `false` | Enable the COSMIC compositor |
| `package` | package | `pkgs.cosmic-comp` | Compositor package to use |
| `xkb.layout` | string | `"us"` | XKB keyboard layout |
| `xkb.variant` | string | `""` | XKB layout variant |
| `xkb.options` | string | `""` | XKB options |
| `xkb.model` | string | `""` | XKB keyboard model |
| `extraConfig` | attrs | `{}` | Additional cosmic-config settings |

The Home Manager module writes XKB configuration to `~/.config/cosmic-comp/v1/xkb-config` when non-default keyboard settings are specified.

## Full Remote Desktop Stack

For a complete remote desktop setup, you need all three components:

```
RDP Client  -->  cosmic-rdp-server  -->  Portal (RemoteDesktop)  -->  Compositor (EIS)
                                    -->  Portal (ScreenCast)     -->  PipeWire streams
```

### NixOS example (all three components)

```nix
{
  inputs = {
    cosmic-rdp-server.url = "github:olafkfreund/cosmic-rdp-server";
    xdg-desktop-portal-cosmic.url = "github:olafkfreund/xdg-desktop-portal-cosmic";
    cosmic-comp.url = "github:olafkfreund/cosmic-comp-rdp";
  };

  outputs = { self, nixpkgs, cosmic-rdp-server, xdg-desktop-portal-cosmic, cosmic-comp, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        cosmic-rdp-server.nixosModules.default
        xdg-desktop-portal-cosmic.nixosModules.default
        cosmic-comp.nixosModules.default
        {
          nixpkgs.overlays = [
            cosmic-rdp-server.overlays.default
            xdg-desktop-portal-cosmic.overlays.default
            cosmic-comp.overlays.default
          ];

          # Compositor with EIS support
          services.cosmic-comp.enable = true;

          # Portal with RemoteDesktop interface
          services.xdg-desktop-portal-cosmic.enable = true;

          # RDP server
          services.cosmic-rdp-server = {
            enable = true;
            openFirewall = true;
            settings.bind = "0.0.0.0:3389";
          };
        }
      ];
    };
  };
}
```

## Related Projects

| Project | Description |
|---------|-------------|
| [cosmic-rdp-server](https://github.com/olafkfreund/cosmic-rdp-server) | RDP server daemon using the portal for capture and input |
| [xdg-desktop-portal-cosmic](https://github.com/olafkfreund/xdg-desktop-portal-cosmic) | Portal backend with RemoteDesktop interface |
| [cosmic-epoch](https://github.com/pop-os/cosmic-epoch) | COSMIC Desktop Environment |
| [cosmic-comp](https://github.com/pop-os/cosmic-comp) | Upstream COSMIC compositor |

## License

GPL-3.0-only
