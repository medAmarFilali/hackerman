import { REGEXP_ONLY_DIGITS } from "input-otp";
import { CircleCheck, CircleX, Loader2 } from "lucide-react";
import { useEffect, useState } from "react";

import { PairingArt } from "@/components/Art";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { InputOTP, InputOTPGroup, InputOTPSlot } from "@/components/ui/input-otp";
import type { PairingView } from "@/lib/types";
import { send } from "@/store";

const CODE_LENGTH = 6;
/** How long "Paired" stays up before the dialog closes itself. */
const SUCCESS_MS = 1400;

export function PairingDialog({ pairing }: { pairing: PairingView | null }) {
  const dismiss = () => send({ type: "dismissPairing" });
  const stage = pairing?.stage.stage;

  useEffect(() => {
    if (stage !== "paired") return;
    const timer = setTimeout(dismiss, SUCCESS_MS);
    return () => clearTimeout(timer);
  }, [stage]);

  return (
    <Dialog open={pairing !== null} onOpenChange={(open) => !open && dismiss()}>
      <DialogContent className="sm:max-w-md">{pairing && <Stage pairing={pairing} />}</DialogContent>
    </Dialog>
  );
}

function Stage({ pairing }: { pairing: PairingView }) {
  const peer = pairing.peerName ?? "the other PC";
  const { stage } = pairing;

  switch (stage.stage) {
    case "connecting":
      return <Waiting title="Connecting…" description="Reaching the other PC." />;
    case "enterCode":
      return <EnterCode peer={peer} />;
    case "showCode":
      return (
        <>
          <PairingArt live={false} />
          <DialogHeader>
            <DialogTitle>Pair with {peer}?</DialogTitle>
            <DialogDescription>
              Type this code on {peer} to let it share a keyboard and mouse with this PC. If you
              didn't start this, close this window.
            </DialogDescription>
          </DialogHeader>
          <p
            aria-label={`Pairing code: ${stage.code.split("").join(" ")}`}
            className="flex justify-center gap-1.5 py-1 select-text"
          >
            {stage.code.split("").map((digit, index) => (
              <span
                key={index}
                className="flex h-12 w-10 items-center justify-center rounded-md border bg-background font-mono text-2xl font-medium tabular-nums"
              >
                {digit}
              </span>
            ))}
          </p>
        </>
      );
    case "verifying":
      return <Waiting title="Checking the code…" description={`Confirming with ${peer}.`} />;
    case "paired":
      return (
        <>
          <PairingArt live />
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <CircleCheck aria-hidden className="size-5 text-signal" strokeWidth={1.5} />
              Paired with {peer}
            </DialogTitle>
            <DialogDescription>Arrange its screens to choose where your cursor crosses.</DialogDescription>
          </DialogHeader>
        </>
      );
    case "failed":
      return (
        <>
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <CircleX aria-hidden className="size-5 text-destructive" />
              Couldn't pair
            </DialogTitle>
            <DialogDescription>{stage.reason}</DialogDescription>
          </DialogHeader>
          <Button className="self-end" variant="outline" onClick={() => send({ type: "dismissPairing" })}>
            Close
          </Button>
        </>
      );
  }
}

function Waiting({ title, description }: { title: string; description: string }) {
  return (
    <DialogHeader>
      <DialogTitle className="flex items-center gap-2">
        <Loader2 aria-hidden className="size-5 animate-spin text-muted-foreground" />
        {title}
      </DialogTitle>
      <DialogDescription>{description}</DialogDescription>
    </DialogHeader>
  );
}

function EnterCode({ peer }: { peer: string }) {
  const [code, setCode] = useState("");
  return (
    <>
      <PairingArt live={false} />
      <DialogHeader>
        <DialogTitle>Enter the code from {peer}</DialogTitle>
        <DialogDescription>
          {peer} is now showing a {CODE_LENGTH}-digit code. Type it here to pair the two PCs.
        </DialogDescription>
      </DialogHeader>
      <div className="flex justify-center py-2">
        <InputOTP
          autoFocus
          aria-label="Pairing code"
          maxLength={CODE_LENGTH}
          pattern={REGEXP_ONLY_DIGITS}
          value={code}
          onChange={setCode}
          onComplete={(complete: string) => send({ type: "submitPairingCode", code: complete })}
        >
          <InputOTPGroup>
            {Array.from({ length: CODE_LENGTH }, (_, index) => (
              <InputOTPSlot key={index} index={index} className="size-11 text-lg" />
            ))}
          </InputOTPGroup>
        </InputOTP>
      </div>
    </>
  );
}
