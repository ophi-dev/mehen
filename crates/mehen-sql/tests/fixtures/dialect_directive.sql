-- sqlfluff:dialect:postgres
-- A file that pins its own dialect via the SQLFluff in-file directive.
SELECT id, name
FROM users
WHERE created_at > now() - interval '7 days'
RETURNING id;
