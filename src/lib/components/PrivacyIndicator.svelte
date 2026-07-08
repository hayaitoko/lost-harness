<script lang="ts">
  // Lost Harness — Privacy routing indicator (traffic-light dot).
  //
  // Three states from the §7 privacy gate:
  //   allow       → green,  "Public"   (the message may leave this machine)
  //   route_local → yellow, "Auto"     (the gate kept it local — e.g. local provider only)
  //   block       → red,    "Blocked"  (the gate vetoed the request)
  //
  // The `binding` prop is the user's intent for the conversation
  // (auto | public | private); the `decision` prop is what the gate did.
  // The label reflects the decision when one is present, falling back to
  // the binding so the chip is never empty.

  type Binding = "auto" | "public" | "private";
  type Decision = null | "allow" | "route_local" | "block";

  interface Props {
    binding?: Binding;
    decision?: Decision;
  }

  let { binding = "auto", decision = null }: Props = $props();

  // Map (decision, binding) → visible label + dot color.
  // Pure derived — no need for $derived unless we want it.
  const spec = $derived.by(() => {
    if (decision === "allow") {
      return { label: "Public", color: "bg-emerald-500", ring: "ring-emerald-500/30" };
    }
    if (decision === "route_local") {
      return { label: "Auto", color: "bg-amber-500", ring: "ring-amber-500/30" };
    }
    if (decision === "block") {
      return { label: "Blocked", color: "bg-rose-500", ring: "ring-rose-500/30" };
    }
    // No decision yet → fall back to the binding.
    if (binding === "private") {
      return { label: "Private", color: "bg-amber-500", ring: "ring-amber-500/30" };
    }
    if (binding === "public") {
      return { label: "Public", color: "bg-emerald-500", ring: "ring-emerald-500/30" };
    }
    return { label: "Auto", color: "bg-neutral-500", ring: "ring-neutral-500/30" };
  });

  const title = $derived.by(() => {
    if (decision) return `${spec.label} (${decision})`;
    return `${spec.label} (${binding})`;
  });
</script>

<span
  class="privacy-indicator inline-flex items-center gap-1.5 rounded-full bg-neutral-900/70 px-2 py-0.5 text-[11px] font-medium text-neutral-300 ring-1 ring-inset {spec.ring}"
  data-binding={binding}
  data-decision={decision ?? "none"}
  role="status"
  aria-label={`Routing: ${spec.label}`}
  {title}
>
  <span
    class="dot inline-block h-2 w-2 rounded-full {spec.color}"
    aria-hidden="true"
  ></span>
  {spec.label}
</span>
