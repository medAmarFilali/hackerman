import { PairingArt } from "@/components/Art";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { formatBytes } from "@/lib/format";
import type { UiState } from "@/lib/types";
import { send } from "@/store";

/** Shown when another PC sends files and Settings says to ask about it. */
export function IncomingFilesDialog({ state }: { state: UiState }) {
  const incoming = state.incomingFiles;
  const folder = state.rules.dropFolder ?? state.desktopFolder;
  const canAllow = state.rules.receiveFiles === "askFirst";
  const answer = (accept: boolean, always = false) => {
    if (incoming) {
      send({ type: "answerIncomingFiles", id: incoming.id, accept, always });
    }
  };

  return (
    <Dialog open={incoming !== null} onOpenChange={(open) => !open && answer(false)}>
      <DialogContent className="sm:max-w-lg">
        {incoming && (
          <>
            <PairingArt live />
            <DialogHeader>
              <DialogTitle>{incoming.peerName} wants to send you files</DialogTitle>
              <DialogDescription>
                {incoming.label}, {formatBytes(incoming.totalBytes)}.
                {folder && ` They'd be saved in ${folder}.`}
                {canAllow && ` If you always allow ${incoming.peerName}, it won't need to ask again. You can change that in Settings.`}
              </DialogDescription>
            </DialogHeader>
            <DialogFooter>
              <Button variant="outline" onClick={() => answer(false)}>
                Turn them down
              </Button>
              {canAllow ? (
                <>
                  <Button variant="outline" onClick={() => answer(true)}>
                    Just this time
                  </Button>
                  <Button onClick={() => answer(true, true)}>Save and always allow</Button>
                </>
              ) : (
                <Button onClick={() => answer(true)}>Save them</Button>
              )}
            </DialogFooter>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
