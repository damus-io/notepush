{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = [
    pkgs.cargo
    pkgs.openssl
    pkgs.pkg-config
    pkgs.websocat
  ];
}
