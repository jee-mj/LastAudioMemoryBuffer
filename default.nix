# default.nix — LAMB v0.2.0 Rust daemon package
{
  lib,
  rustPlatform,
  pipewire,
  jack2,
  pkg-config,
}:

rustPlatform.buildRustPackage {
  pname = "lamb";
  version = "0.2.0";

  src = ./.;
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];
  buildInputs = [ pipewire jack2 ];

  doCheck = true;

  meta = with lib; {
    description = "LastAudioMemoryBuffer rolling audio daemon";
    longDescription = ''
      LAMB continuously records audio into a bounded rolling memory buffer and
      recalls recent audio on command. v0.2 is implemented as a Rust daemon and
      CLI with split-safe per-channel WAV export.  Supports PipeWire and JACK
      capture backends.
    '';
    homepage = "https://github.com/jee-mj/LastAudioMemoryBuffer";
    license = licenses.gpl3Only;
    platforms = platforms.linux;
    mainProgram = "lamb";
  };
}
