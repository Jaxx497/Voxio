# Changelog

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

