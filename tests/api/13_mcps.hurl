# The MCP server registry, over a live server. What is checked here is who may register one,
# what the types refuse at the door, and that a token written is never read back — not counts,
# which would move whenever an earlier file changes.

POST {{host}}/_ip/login
{"user": "root", "passphrase": "{{passphrase}}"}
HTTP 204

# A server requiring nothing, which is what one that says nothing requires.
PUT {{host}}/_ip/mcps/warehouse
x-ip-csrf: 1
{
  "name": "Warehouse",
  "endpoint": "https://mcp.example.com/warehouse"
}
HTTP 201
[Asserts]
jsonpath "$.id" == "warehouse"
jsonpath "$.name" == "Warehouse"
jsonpath "$.endpoint" == "https://mcp.example.com/warehouse"
jsonpath "$.auth" == "none"
jsonpath "$.created_at" isInteger

# One holding a credential. The token goes in and is never reported back, the way a tenant
# key and a passphrase are: what a reader needs is that one is required, not what it is.
PUT {{host}}/_ip/mcps/ledger
x-ip-csrf: 1
{
  "name": "Ledger",
  "endpoint": "https://mcp.example.com/ledger",
  "auth": {"kind": "bearer", "token": "sk-ledger-secret"}
}
HTTP 201
[Asserts]
jsonpath "$.auth" == "bearer"
jsonpath "$.token" not exists

GET {{host}}/_ip/mcps/ledger
HTTP 200
[Asserts]
jsonpath "$.auth" == "bearer"
jsonpath "$.token" not exists
body not contains "sk-ledger-secret"

GET {{host}}/_ip/mcps
HTTP 200
[Asserts]
jsonpath "$[*].id" contains "warehouse"
jsonpath "$[*].id" contains "ledger"

# Writing again replaces what was there, including dropping a credential it no longer needs.
PUT {{host}}/_ip/mcps/warehouse
x-ip-csrf: 1
{
  "name": "Warehouse EU",
  "endpoint": "https://mcp.eu.example.com/warehouse"
}
HTTP 201
[Asserts]
jsonpath "$.name" == "Warehouse EU"
jsonpath "$.endpoint" == "https://mcp.eu.example.com/warehouse"

GET {{host}}/_ip/mcps/nothing-here
HTTP 404

# A name the type refuses, which is the caller's own mistake and not ours.
PUT {{host}}/_ip/mcps/NOT%20AN%20ID
x-ip-csrf: 1
{
  "name": "Whatever",
  "endpoint": "https://mcp.example.com/whatever"
}
HTTP 422

# An endpoint that is not one.
PUT {{host}}/_ip/mcps/broken
x-ip-csrf: 1
{
  "name": "Broken",
  "endpoint": "not a url"
}
HTTP 422

# A field nothing reads, refused rather than ignored, so a typo is news at the door.
PUT {{host}}/_ip/mcps/broken
x-ip-csrf: 1
{
  "name": "Broken",
  "endpoint": "https://mcp.example.com/broken",
  "tools": ["sql"]
}
HTTP 422

# A browser that sends its cookie without the header a page cannot forge.
PUT {{host}}/_ip/mcps/unproven
{
  "name": "Unproven",
  "endpoint": "https://mcp.example.com/unproven"
}
HTTP 403

DELETE {{host}}/_ip/mcps/ledger
x-ip-csrf: 1
HTTP 204

GET {{host}}/_ip/mcps/ledger
HTTP 404

# Removing one that is already gone is reported missing rather than pretended.
DELETE {{host}}/_ip/mcps/ledger
x-ip-csrf: 1
HTTP 404

# A registered server a template still names is left alone: a tool that cannot be reached is
# refused when a run asks for it, not when the server it lives on is removed.
DELETE {{host}}/_ip/mcps/warehouse
x-ip-csrf: 1
HTTP 204
