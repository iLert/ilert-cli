---
name: ilert-terraform
description: Use the ilert Terraform provider and export existing resources as HCL and import blocks when the user requests Terraform work
user-invocable: true
---

# ilert with Terraform

Use this skill when the user's request involves Terraform. Do not introduce
Terraform as part of an unrelated CLI task or migration.

For new resources, write HCL directly. For existing resources, use the export
below. When migrating an existing Terraform setup, read its configuration to
preserve naming, variables and module boundaries, using the relevant migration
skill for resource mappings should external providers be involved.

## Provider setup

Use the official [`iLert/ilert` provider](https://registry.terraform.io/providers/iLert/ilert/latest/docs),
keeping the project's existing version constraints. It reads a user API key
from `ILERT_API_TOKEN`; it cannot reuse the CLI's OAuth login. The CLI uses
`ILERT_API_KEY` for that same key. For non-production environments, set the
provider's `endpoint` / `ILERT_ENDPOINT` to the CLI profile's base URL.

## Export an existing resource

`ilert infrastructure-as-code create` generates code without changing the
account. Pass one complete API entity, including its `id`:

```sh
ilert services get --id 1234 -o json \
  | ilert infrastructure-as-code create --format TERRAFORM --resourceType SERVICE --body - -o json \
  > service.json
jq -r '.[] | .resource, .importBlock' service.json
```

The response includes `resource`, `importBlock` and `importCommand`. Use
`jq -r` to extract HCL; the CLI's `--jq` prints JSON-quoted strings. Merge the
blocks into the target Terraform configuration, checking existing addresses
before adding them again. Import blocks require Terraform ≥ 1.5.

Use `ilert infrastructure-as-code create --help` for supported resource types.
Notable mappings are `ONCALL_SCHEDULE` → `ilert_schedule`,
`ALERT_ACTION_CONNECTOR` → `ilert_connector`, and
`METRIC_PROVIDER` → `ilert_metric_data_source`. For resources the transform
does not support, use the provider docs for HCL and import syntax.

Before importing:

- Fetch optional fields with `--include`, such as schedule `scheduleLayers`
  (`shifts` for static schedules), status-page `groups`, and alert-action
  `conditions`. Missing fields can lead to removals in the plan.
- Check generated names for collisions; users all default to `ilert_user.user`.
  Rename both the resource label and its import target. Replace literal IDs
  with Terraform references where the related resources are in the configuration.
- Check omitted empty, `false` and `0` values against provider defaults.
- Connector secrets can appear in both the JSON and HCL. Keep raw exports out
  of commits and use sensitive variables for secrets; values can still be
  stored in Terraform state.

In the intended Terraform root and workspace, initialize only if needed, format and
validate the configuration, then save and inspect `terraform plan -out=import.tfplan`.
An import-only plan should show the expected imports with **0 to add, 0 to
change, 0 to destroy**. Resolve unintended changes before applying that saved
plan within the user's requested scope. Verify a no-change plan afterward.
Always confirm with the user before running apply.
