The migration is ready, but `users.email` has 37 duplicate rows in the staging copy. Before I add the unique index I need to know what to do with them:

1. Keep the newest row per email and delete the rest
2. Merge them into the oldest row
3. Skip the unique index for now

Which one?
