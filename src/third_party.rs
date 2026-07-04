//! Third-party attribution and license notices.
//!
//! Centralizes the notices we are required to present to end users so the server
//! and the GUI display identical text. The Cisco OpenH264 binary license
//! (`BINARY_LICENSE.txt` v1.0) requires an attribution string wherever the user
//! controls the codec (condition 3) and reproduction of the full license text in
//! a licensing or about location (condition 4).
//!
//! Cisco's OpenH264 binary is downloaded and installed separately by the user;
//! this project never bundles, compiles, or auto-downloads it. See
//! `docs/decisions/H264-CODEC-STRATEGY.md` and `docs/OPENH264-SETUP.md`.

/// Attribution required by the Cisco OpenH264 binary license (condition 3),
/// shown wherever the end user controls the H.264 codec.
pub const OPENH264_ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

/// Where the user obtains Cisco's OpenH264 binary (Cisco's own release list).
pub const OPENH264_RELEASES_URL: &str = "https://github.com/cisco/openh264/releases";

/// Full verbatim Cisco OpenH264 binary license (condition 4).
pub const OPENH264_BINARY_LICENSE: &str = include_str!("../licenses/OpenH264-BINARY_LICENSE.txt");

/// Assemble the third-party license notices for the `--licenses` output.
#[must_use]
pub fn notices() -> String {
    format!(
        "lamco-rdp-server third-party license notices\n\
         ============================================\n\
         \n\
         This product can use the OpenH264 Video Codec for software H.264\n\
         encoding. {OPENH264_ATTRIBUTION}\n\
         \n\
         Cisco's OpenH264 binary is downloaded and installed separately by the\n\
         user; it is not bundled with, compiled into, or auto-downloaded by this\n\
         software. Obtain it from your distribution's package or from\n\
         {OPENH264_RELEASES_URL}.\n\
         \n\
         The full Cisco OpenH264 binary license follows.\n\
         \n\
         --------------------------------------------------------------------\n\
         {OPENH264_BINARY_LICENSE}"
    )
}
