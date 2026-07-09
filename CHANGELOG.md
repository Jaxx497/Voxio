# Changelog

## [0.2.3] - Volume Adjuster & Example Fixes

*Minor fixes for the recently implemented volume adjustment.*

### Changed:
  - Raised volume ceiling from 1.2 to 1.5 (~+7 dB) on the perceptual scale
  - Boosted peaks are now hard-clipped to [-1, 1] before reaching the
    output device and visualization tap

### Fixed:
  - Removed file logging (event + panic logs) from the interactive example

## [0.2.2] - Volume Adjusters
### Added:
  - Volume adjustment
  - Enhanced device recovery logic

### Changed: 
  - Bumped crossbeam-channel version

## [0.2.1] - Decoder Improvements + Waveform Module

*Lots of little tunings + a new waveform bins generation feature*

#### Added:
  - New (optional) waveform bin generation module
  - draw_waveform example added to example suite
  - Improved seeking logic
  - DurationResolved type reporting back sample-accurate durations

#### Fixed:
  - Provide better handling for files with invalid or missing headers
  - More decoder resilience against poorly encoded files
  - Removed hardcoded grace period for bad packets to recover from
  - Provide event to correct durations that were calcualated improperly due
    invalid or missing headers
  - Corrupt packets at end of file end playback with EOF event instead of error
  - Removed recoverable decoder error events; errors are now silently retried
    internally
  - Ensure that no division by 0 errors can ever occur


## [0.2.0] - Complete rewrite
### Voxio has been rewritten from the ground up.

  - VOXIO INTERNALLY MANAGES DEVICE SWITCH!
  - Users can manipulate internal defaults with the `Vox::new_with_config()` method
  - Voxio has the ability to read and manipulate track volume
  - based on ReplayGain tags
  - API provides an event recieving module
  -     - matach against a series of events rather than polling for changes in
  -       state
  - TapHandle gives reader full control over the samples passed
  - Most public methods are no longer result types
  - New testing suite established with real examples

## [0.1.6] Expose `clear_next()` method in public api

#### Added
  - clear_next method allows users to clear queued track

## [0.1.5] Push to symphonia version 0.6.0

#### Added
 - Improved seek logic
 - Better webm support

#### Fixed
 - Bumped symphonia-adapter-opus version

## [0.1.4]

#### Added
 - Proper documentation for crates.io

#### Fixed
 - Stability improvements for webm files
 - Pause command occasionally overwridden

