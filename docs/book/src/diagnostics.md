# Diagnostics

Most errors and warnings provided by `wgsl-analyzer` come from `wgsl-analyzer`'s own analysis.
Some of these diagnostics do not respect `// wgsl-analyzer` diagnostic control comments yet.
They can be turned off using the `wgsl-analyzer.diagnostics.enable`, `wgsl-analyzer.diagnostics.experimental.enable`, or `wgsl-analyzer.diagnostics.disabled` settings.

{{#include diagnostics_generated.md:2:}}

## Pull diagnostics

`wgsl-analyzer` supports the pull diagnostics requests from version 3.17 of the Language Server Protocol.
With `textDocument/diagnostic` the editor asks for the diagnostics of one file.
With `workspace/diagnostic` it asks for the diagnostics of every `.wgsl` and `.wesl` file in your project, open or not.
Every `.wgsl` and `.wesl` file you have open is checked too, so a file in one of your project's dependencies is checked while it is open.
Workspace pulls are how errors in files you have not opened reach the editor, for example the VS Code Problems panel.
VS Code uses both requests, and the extension needs no setting for them.
An editor without pull support gets `textDocument/publishDiagnostics` notifications instead, for open files only.

A report with diagnostics carries a result id, a hash of the diagnostics the server sent for that file and of whether the file is open.
The editor sends the id back with its next pull, and when the file's diagnostics have not changed, the server answers that the file is unchanged instead of sending the diagnostics again.
An id the editor got while a file was open does not match once the file is closed, so the next workspace pull sends the file's diagnostics in full.
That way, if you fix an error in the editor and then close the file without saving, the error usually shows up again.
A file with no diagnostics has no result id.
If the editor sent a result id for a file that now has no diagnostics, the file comes back with an empty list and no result id.
So does a file with a result id that the workspace pull does not check, such as a deleted file or a dependency file you have closed.
The editor then clears what it shows for the file and stops sending an id for it.

When a workspace pull carries a `partialResultToken` (VS Code sends one), the server sends the files with new or changed diagnostics as partial results (`$/progress` notifications) during the check.
The editor can show them before the whole workspace is checked.
The final response carries the rest: the unchanged files, the files to clear, and any files with new or changed diagnostics not sent yet.
This departs from the specification's general rule that the response after partial results carries no results.
A client must apply the entries in the response as well as the partial results; VS Code does.
Without a `partialResultToken`, the whole report comes in the response.

A workspace pull with nothing new for the editor (every file unchanged, or nothing to report) is not answered right away.
The server holds it open and checks again when something changes: a file is edited, created, or deleted; a file is opened or closed in the editor; a setting changes; or the project finishes loading.
It answers once a check finds something new.
VS Code sends a new workspace pull two seconds after each answer, so if the server answered at once, it would check the whole workspace every two seconds while you are only reading code.
A workspace pull that arrives while the server is still loading the project's files is not held: it is answered at once with an empty report.
The editor's first pull after loading finishes gets the diagnostics.

For editor integrators: do not put a timeout on `workspace/diagnostic`, since a held pull can stay open for as long as nothing changes.
To end a held pull, cancel it with `$/cancelRequest`, and the server answers it with a `RequestCancelled` error (VS Code cancels a pending workspace pull before it sends another).
On `shutdown`, the server answers a held pull with an empty report.
