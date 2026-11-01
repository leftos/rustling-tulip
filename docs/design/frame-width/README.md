# Pane frame width (RT-651)

## Question

The owner: "The colored session border is unnecessarily thick. Maybe that can be another customizable appearance element, with a lower default value?" The band was the terminal's whole 6 px padding ring, painted in the frame colour.

`index.html` shows the options. Question 1 offered default widths of 6 px (as it was), 3 px, 2 px, 1 px and 0. Question 2 asked what a thinner frame does to the text: A, the ring shrinks and the text moves in; B, a band of the chosen width, then the terminal background out to the old 6 px inset.

## Answer

- The default is 1 px.
- B: the text keeps its 6 px inset at every width, so the PTY grid size never changes with the width.
- The width is a field of the appearance cascade (session, repo or workspace, app, built-in), set from 0 to 6 px with a stepper row in the appearance editor.

Recorded on RT-651 and in `docs/native-client.md` ("The frame round a session's terminal…").
