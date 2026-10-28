Fixed a profile of workers read from outside, on Linux, dropping a worker
after one failed read, which could leave it with no samples while requests
were starting and ending: a failed read is now skipped and the worker
sampled again, and a stacks read is tried again.
