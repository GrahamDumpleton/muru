Added optional worker groups: a group with `optional = true` is one the
release is ready without. Its workers join as each listens, its requests
wait for one until the queue timeout rather than going to the default
group, and a worker of it that cannot start is given up instead of
stopping the release.
