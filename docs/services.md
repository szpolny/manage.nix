# Inspecting services

Open a machine from **Machines**, then use **Services** to search by name or
description and filter running, stopped, failed, or other states. Expand a
service's details to see its native active state, substate, and load state.

NixOS agents report systemd service units loaded by the system manager,
including inactive units. Active services that have finished (such as oneshot
jobs) and transitional states appear under **Other**. Units that have never been
loaded are outside this snapshot. macOS agents report the system launchd jobs
visible to the root agent, with PID and last exit status in the details. A
nonzero last exit status without a running PID appears as failed. User session
services are not included.

The last inspection time appears above the list. Inspections refresh with agent
reports; offline machines retain their last snapshot and display an offline
notice. A collection failure shows unavailable rather than an empty successful
list. Older agents show unavailable until upgraded. Inspecting services is
read-only.
