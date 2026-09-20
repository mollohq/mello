// Logging for code that runs inside somebody else's process.
//
// Rules this file exists to keep:
//  - Nothing here is ever called from the game's present path. The present path
//    records numbers in shared memory; the hook thread writes the words.
//  - A log line never allocates from the game's heap and never takes a lock the
//    game could be holding. It formats into a stack buffer and writes once.
//  - A failure to log is never a failure. Every call here can do nothing.

#pragma once

namespace mello_hook {

// Opens the log file for this process. Safe to call more than once. The name
// carries the process id, because one client can hook several games.
void log_open(const char* role);

void log_line(const char* format, ...);

void log_close();

}  // namespace mello_hook
