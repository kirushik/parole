{
  description = "refview: a read-only reviewer for sandbox-written git refs, built on Rune";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      # Rune pins this toolchain in its rust-toolchain.toml.
      rust = pkgs.rust-bin.stable."1.99.1".default;
      # winit, wgpu and fontique open these at run time.
      libs = with pkgs; [
        libxkbcommon
        wayland
        vulkan-loader
        fontconfig
        libx11
        libxcursor
        libxi
        libxrandr
      ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        # git: the app shells out to it, and so do the tests and scripts.
        nativeBuildInputs = [ rust pkgs.pkg-config pkgs.git ];
        buildInputs = libs;
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libs;
      };
    };
}
