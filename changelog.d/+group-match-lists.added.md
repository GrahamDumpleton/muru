Added `host` and `method` lists to a worker group's `match` table: a
group takes a request when its path is under one of the group's paths
and every list given names the request's client class, host or method.
A group asking more of a request comes before one asking less, and two
groups asking the same of the same path are refused.
