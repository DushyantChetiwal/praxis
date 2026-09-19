#!/usr/bin/env sh
set -eu

# Downloads a tarball from https://zed.dev/releases and unpacks it
# into ~/.local/. If you'd prefer to do this manually, instructions are at
# https://zed.dev/docs/linux.

main() {
    platform="$(uname -s)"
    arch="$(uname -m)"
    channel="${ZED_CHANNEL:-stable}"
    ZED_VERSION="${ZED_VERSION:-latest}"
    # Use TMPDIR if available (for environments with non-standard temp directories)
    if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
        temp="$(mktemp -d "$TMPDIR/zed-XXXXXX")"
    else
        temp="$(mktemp -d "/tmp/zed-XXXXXX")"
    fi

    if [ "$platform" = "Darwin" ]; then
        platform="macos"
    elif [ "$platform" = "Linux" ]; then
        platform="linux"
    else
        echo "Unsupported platform $platform"
        exit 1
    fi

    case "$platform-$arch" in
        macos-arm64* | linux-arm64* | linux-aarch64)
            arch="aarch64"
            ;;
        macos-x86* | linux-x86*)
            arch="x86_64"
            ;;
        *)
            echo "Unsupported platform or architecture"
            exit 1
            ;;
    esac

    if command -v curl >/dev/null 2>&1; then
        curl () {
            command curl -fL "$@"
        }
    elif command -v wget >/dev/null 2>&1; then
        curl () {
            wget -O- "$@"
        }
    else
        echo "Could not find 'curl' or 'wget' in your path"
        exit 1
    fi

    "$platform" "$@"

    if [ "$(command -v "$cli_name")" = "$HOME/.local/bin/$cli_name" ]; then
        echo "$app_name has been installed. Run with '$cli_name'"
    else
        echo "To run $app_name from your terminal, you must add ~/.local/bin to your PATH"
        echo "Run:"

        case "$SHELL" in
            *zsh)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
                echo "   source ~/.zshrc"
                ;;
            *fish)
                echo "   fish_add_path -U $HOME/.local/bin"
                ;;
            *)
                echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
                echo "   source ~/.bashrc"
                ;;
        esac

        echo "To run $app_name now, '~/.local/bin/$cli_name'"
    fi
}

linux() {
    suffix=""
    app_slug="zed"
    cli_name="zed"
    app_name="Zed"
    if [ "$channel" != "stable" ]; then
        suffix="-$channel"
    fi
    if [ "$channel" = "dev" ]; then
        app_slug="praxis"
        cli_name="praxis"
        app_name="Praxis"
    fi
    archive="$temp/${app_slug}-linux-$arch.tar.gz"

    if [ -n "${ZED_BUNDLE_PATH:-}" ]; then
        cp "$ZED_BUNDLE_PATH" "$archive"
    elif [ "$channel" = "dev" ]; then
        echo "Downloading the latest Praxis Dev release"
        curl "https://github.com/DushyantChetiwal/praxis/releases/latest/download/praxis-linux-$arch.tar.gz" > "$archive"
    else
        echo "Downloading Zed version: $ZED_VERSION"
        curl "https://cloud.zed.dev/releases/$channel/$ZED_VERSION/download?asset=zed&arch=$arch&os=linux&source=install.sh" > "$archive"
    fi

    appid=""
    case "$channel" in
      stable)
        appid="dev.zed.Zed"
        ;;
      nightly)
        appid="dev.zed.Zed-Nightly"
        ;;
      preview)
        appid="dev.zed.Zed-Preview"
        ;;
      dev)
        appid="io.github.dushyantchetiwal.Praxis-Dev"
        ;;
      *)
        echo "Unknown release channel: ${channel}. Using stable app ID."
        appid="dev.zed.Zed"
        ;;
    esac

    # Unpack
    rm -rf "$HOME/.local/${app_slug}${suffix}.app"
    mkdir -p "$HOME/.local/${app_slug}${suffix}.app"
    tar -xzf "$archive" -C "$HOME/.local/"

    zed_editor="$HOME/.local/${app_slug}${suffix}.app/libexec/zed-editor"
    if [ -f "$zed_editor" ] && command -v ldd >/dev/null 2>&1; then
        missing="$(ldd "$zed_editor" 2>/dev/null | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
        if [ -n "$missing" ]; then
            echo "Warning: your system is missing libraries that $app_name needs:"
            echo "$missing" | sed 's/^/    /'
            echo "Install them with your package manager, or $app_name will fail to start."
        fi
    fi

    # Setup ~/.local directories
    mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"

    # Link the binary
    if [ -f "$HOME/.local/${app_slug}${suffix}.app/bin/${cli_name}" ]; then
        ln -sf "$HOME/.local/${app_slug}${suffix}.app/bin/${cli_name}" "$HOME/.local/bin/${cli_name}"
    else
        # support for versions before 0.139.x.
        ln -sf "$HOME/.local/${app_slug}${suffix}.app/bin/cli" "$HOME/.local/bin/${cli_name}"
    fi

    # Copy .desktop file
    desktop_file_path="$HOME/.local/share/applications/${appid}.desktop"
    src_dir="$HOME/.local/${app_slug}${suffix}.app/share/applications"
    if [ -f "$src_dir/${appid}.desktop" ]; then
        cp "$src_dir/${appid}.desktop" "${desktop_file_path}"
    else
        # Fallback for older tarballs
        cp "$src_dir/${app_slug}${suffix}.desktop" "${desktop_file_path}"
    fi
    sed -i "s|Icon=${app_slug}|Icon=$HOME/.local/${app_slug}${suffix}.app/share/icons/hicolor/512x512/apps/${app_slug}.png|g" "${desktop_file_path}"
    sed -i "s|Exec=${cli_name}|Exec=$HOME/.local/${app_slug}${suffix}.app/bin/${cli_name}|g" "${desktop_file_path}"
}

macos() {
    cli_name="zed"
    app_name="Zed"
    if [ "$channel" = "dev" ]; then
        echo "Downloading the latest Praxis Dev release"
        cli_name="praxis"
        app_name="Praxis"
        disk_image="$temp/Praxis-$arch.dmg"
        curl -L "https://github.com/DushyantChetiwal/praxis/releases/latest/download/Praxis-$arch.dmg" > "$disk_image"
    else
        echo "Downloading Zed version: $ZED_VERSION"
        disk_image="$temp/Zed-$arch.dmg"
        curl "https://cloud.zed.dev/releases/$channel/$ZED_VERSION/download?asset=zed&os=macos&arch=$arch&source=install.sh" > "$disk_image"
    fi
    hdiutil attach -quiet "$disk_image" -mountpoint "$temp/mount"
    app="$(cd "$temp/mount/"; echo *.app)"
    echo "Installing $app"
    if [ -d "/Applications/$app" ]; then
        echo "Removing existing $app"
        rm -rf "/Applications/$app"
    fi
    ditto "$temp/mount/$app" "/Applications/$app"
    hdiutil detach -quiet "$temp/mount"

    mkdir -p "$HOME/.local/bin"
    # Link the binary
    ln -sf "/Applications/$app/Contents/MacOS/cli" "$HOME/.local/bin/${cli_name}"
}

main "$@"
