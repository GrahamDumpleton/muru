Fixed the dashboard's requests per second, which stopped at 16.7 under load
because the rate was taken over a full minute while the in-memory access
log only held the last 1000 requests.
