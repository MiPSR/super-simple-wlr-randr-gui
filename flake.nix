{
  description = "Native wlr-randr display settings (Rust/Rune, Wayland)";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      libs = with pkgs; [ wayland libxkbcommon vulkan-loader fontconfig freetype expat libGL libx11 libxcursor libxrandr libxi ];
      libPath = pkgs.lib.makeLibraryPath libs;
    in {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [ rustc cargo rustfmt clippy gcc pkg-config desktop-file-utils wlr-randr ];
        buildInputs = libs;
        shellHook = ''
          export LD_LIBRARY_PATH="${libPath}:''${LD_LIBRARY_PATH:-}"
          export CARGO_BUILD_JOBS=2
        '';
      };
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "super-simple-wlr-randr-gui";
        version = "0.1.0";
        src = pkgs.lib.cleanSourceWith {
          src = pkgs.lib.cleanSource ./.;
          filter = path: type: baseNameOf path != "target";
        };
        cargoHash = "sha256-xFnHMcv1Hxz7MHoGAuozlho1kWoSXR9zZF2rEhaPymM=";
        nativeBuildInputs = with pkgs; [ pkg-config makeWrapper desktop-file-utils ];
        buildInputs = libs;
        doCheck = true;
        checkFlags = [ "--test-threads=1" ];
        postInstall = ''
          install -Dm644 assets/io.github.MiPSR.SuperSimpleRandR.desktop $out/share/applications/io.github.MiPSR.SuperSimpleRandR.desktop
          install -Dm644 assets/super-simple-randr.svg $out/share/icons/hicolor/scalable/apps/io.github.MiPSR.SuperSimpleRandR.svg
          desktop-file-validate $out/share/applications/io.github.MiPSR.SuperSimpleRandR.desktop
          ln -s ssrr $out/bin/super-simple-wlr-randr-gui
        '';
        postFixup = ''
          wrapProgram $out/bin/ssrr --prefix LD_LIBRARY_PATH : "${libPath}" --prefix PATH : "${pkgs.lib.makeBinPath [ pkgs.wlr-randr ]}"
        '';
        meta = {
          description = "Native Wayland display settings using wlr-randr";
          mainProgram = "ssrr";
          license = pkgs.lib.licenses.agpl3Only;
          platforms = [ system ];
        };
      };
      checks.${system}.default = self.packages.${system}.default;
    };
}
