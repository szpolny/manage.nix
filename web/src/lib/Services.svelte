<script lang="ts">
 import type { ServiceSnapshot } from '#lib/types.ts';
 import { ago, exact } from '#lib/format.ts';
 let { snapshot, online, now }: { snapshot: ServiceSnapshot | null | undefined; online: boolean; now: number } = $props();
 let search = $state('');
 let stateFilter = $state('all');
 let services = $derived((snapshot?.services ?? []).filter(s =>
  (stateFilter === 'all' || s.state === stateFilter) && `${s.name} ${s.description}`.toLowerCase().includes(search.toLowerCase())));
</script>

<section class="section" data-testid="services">
 <h2>Services</h2>
 {#if !snapshot}
  <p class="muted">Service information is unavailable. The agent has not reported a service inspection.</p>
 {:else}
  <p class="muted" title={exact(snapshot.refreshed_at)}>
   Last inspection {ago(snapshot.refreshed_at, now)} ({exact(snapshot.refreshed_at)}).
   {#if !online}Machine offline; this snapshot may be out of date.{:else}Refreshes with agent reports.{/if}
  </p>
  {#if snapshot.error}
   <p class="error" role="status">Service information is unavailable: {snapshot.error}</p>
  {:else}
   <div class="form-row">
    <label>Find service<input type="search" bind:value={search} placeholder="Name or description" /></label>
    <label>State<select bind:value={stateFilter}>
     <option value="all">All states</option><option value="running">Running</option>
     <option value="stopped">Stopped</option><option value="failed">Failed</option><option value="other">Other</option>
    </select></label>
   </div>
   {#if !snapshot.services.length}<p class="muted">No services reported.</p>
   {:else if !services.length}<p class="muted">No services match these filters.</p>
   {:else}
    <div class="table-card"><table>
     <thead><tr><th>Service</th><th>State</th><th>Details</th></tr></thead>
     <tbody>{#each services as service (service.name)}
      <tr><td class="mono">{service.name}</td><td class:error={service.state === 'failed'}>{service.state}</td>
       <td><details><summary>{service.description || 'Inspect status'}</summary>
        <dl class="facts"><dt>Active state</dt><dd>{service.active_state}</dd>
         <dt>Substate</dt><dd>{service.sub_state}</dd><dt>Load state</dt><dd>{service.load_state}</dd></dl>
       </details></td></tr>
     {/each}</tbody>
    </table></div>
   {/if}
  {/if}
 {/if}
</section>
<style>.section { margin-top: 1.75rem; }</style>
