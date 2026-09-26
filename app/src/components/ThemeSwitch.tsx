import { Moon, Sun } from "lucide-react";
import { Switch } from "radix-ui";

import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { chooseTheme, useTheme } from "@/lib/theme";

export function ThemeSwitch() {
  const dark = useTheme((store) => store.theme === "dark");
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <Switch.Root
          aria-label="Dark theme"
          checked={dark}
          onCheckedChange={(checked) => chooseTheme(checked ? "dark" : "light")}
          className="inline-flex h-6 w-11 shrink-0 items-center rounded-full border bg-card p-0.5 outline-none transition-colors hover:border-line focus-visible:ring-3 focus-visible:ring-ring/50"
        >
          <Switch.Thumb className="flex size-[1.125rem] items-center justify-center rounded-full bg-primary text-primary-foreground transition-transform duration-200 ease-out data-[state=checked]:translate-x-5 motion-reduce:transition-none">
            {dark ? <Moon className="size-3" /> : <Sun className="size-3" />}
          </Switch.Thumb>
        </Switch.Root>
      </TooltipTrigger>
      <TooltipContent>{dark ? "Switch to the light theme" : "Switch to the dark theme"}</TooltipContent>
    </Tooltip>
  );
}
