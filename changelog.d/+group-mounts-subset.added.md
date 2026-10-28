Added a worker group's `mounts` list, the named mounts its workers serve
beside the default mount: the host still imports every mount, a worker of
the group realises and declares only its own, and a group's paths must lie
within the mounts it serves.
