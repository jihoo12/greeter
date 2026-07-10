# greetd-mini-greeter

A minimal CLI greeter for [greetd](https://sr.ht/~kennylevinsen/greetd/) that
is meant to just work with zero configuration:

- Auto-discovers Wayland/X11 sessions from the standard `.desktop` locations
  (including the NixOS `/run/current-system/sw/share/{wayland,x}sessions`
  paths). If there's exactly one session installed, it's picked automatically
  — you only type your username and password.
- If no session files are found at all, it falls back to simply starting your
  login shell, so it also works as a plain console login manager.
- Password entry disables terminal echo (not raw mode — backspace etc. still
  work normally) and always restores it afterwards.
- Speaks the real greetd IPC protocol (`greetd_ipc` crate), so it goes
  through PAM exactly like any other greeter (agreety, tuigreet, ...).
- Small dependency footprint: `greetd_ipc`, `shell-words`, `libc`. No async
  runtime, no TUI library.

It intentionally does not try to be `tuigreet`-fancy — no ncurses UI, no
theming. It's meant for people who want the equivalent of a classic `login:` /
`Password:` prompt, but talking to greetd instead of getty+login.

## Building

With flakes:

```sh
nix build
./result/bin/greetd-mini-greeter --help  # (no flags; just documents it runs)
```

Without flakes:

```sh
nix-build
```

Both produce the same binary at `result/bin/greetd-mini-greeter`.

## NixOS integration

### Flakes

In your system flake:

```nix
{
  inputs.greetd-mini-greeter.url = "github:you/greetd-mini-greeter";

  outputs = { self, nixpkgs, greetd-mini-greeter, ... }: {
    nixosConfigurations.yourhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        greetd-mini-greeter.nixosModules.default
        {
          services.greetd-mini-greeter.enable = true;
        }
        ./configuration.nix
      ];
    };
  };
}
```

This enables `services.greetd` and points its `default_session.command` at
the greeter binary. It does **not** enable a display manager session type for
you beyond that — install whatever compositor/WM you want
(`programs.sway.enable = true;`, etc.) and its `.desktop` file will be picked
up automatically at login.

If you want a specific session pre-selected without prompting (e.g. you have
several installed but always want Sway), just don't install the others'
session files on that machine — with exactly one `.desktop` file found, the
greeter skips the selection prompt entirely.

### Without flakes

```nix
let
  greetd-mini-greeter = pkgs.callPackage (pkgs.fetchFromGitHub {
    owner = "you";
    repo = "greetd-mini-greeter";
    rev = "...";
    sha256 = "...";
  }) { };
in
{
  services.greetd = {
    enable = true;
    settings.default_session = {
      command = "${greetd-mini-greeter}/bin/greetd-mini-greeter";
      user = "greeter";
    };
  };
}
```

## Manual / non-NixOS usage

Point greetd's `config.toml` at the built binary:

```toml
[terminal]
vt = 1

[default_session]
command = "/path/to/greetd-mini-greeter"
user = "greeter"
```

Then `systemctl enable --now greetd` (or however your distro starts it).

## How it works

1. Prints the hostname and a `login:` prompt.
2. Discovers sessions from `/run/current-system/sw/share/{wayland,x}sessions`,
   `/usr/share/{wayland,x}sessions`, `/usr/local/share/{wayland,x}sessions`.
   With 0 sessions, falls back to the user's shell; with 1, auto-selects it;
   with 2+, prompts for a number.
3. Opens `$GREETD_SOCK`, sends `create_session`, and relays whatever
   `auth_message` prompts PAM sends back (visible or secret) — it does not
   assume "the only prompt is a password", so it also works with e.g. 2FA/TOTP
   PAM modules.
4. On success, sends `start_session` with the chosen command, then exits so
   greetd can hand off to the session. On failure, it shows the error and
   loops back to the username prompt.

## Known limitations

- No persistent "last used session/username" memory — every boot starts from
  a clean prompt. This is a deliberate simplicity trade-off, not an oversight.
- If the process receives a fatal signal *during* password entry, the
  terminal could theoretically be left with echo disabled until the next
  prompt re-toggles it. The greeter always forces echo back on at startup as
  a safety net for this.
