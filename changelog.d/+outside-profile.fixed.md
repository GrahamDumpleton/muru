Fixed a profile of workers read from outside, on Linux, dropping a worker
after one failed read, which could leave it with no samples while requests
were starting and ending: a failed read is now skipped and the worker
sampled again, and a stacks read is tried again. Such a profile also names
each function by the line it starts on, as a profile read from inside
does, rather than by the line that was running, so a function's samples
are counted together.
