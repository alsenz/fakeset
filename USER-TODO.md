Sometimes it's worth revisting things after an initial pass as you often find deeper improvements second time round.
Let's re-run the code tidy up audit, with a broader scope across everything in lib which is relevant to the last few sets of changes, in view of the development of recent theory.
- Any old or legacy code that contradicts or is contrary to the latest theory
- Any code that needs to be brought in line with the new theory for clarity
- Any code that could be simplified with the new theory.
- Any redundant, or _nearly redundant_ code which could be rolled into code from the new theory, or aligned with new concepts.
- Any simplification and clarity improvements that would be well aligned with the latest theory and conceptual thinking.
- Any repetitive code, from legacy theory or even from later theory, which could be improved by simplifying into one code path.
- Any needlessly complex code paths where simpler options which still align with the conceptual framework and theory would suffice.
- Areas where reusability and modularity could be improved, without compromising or stepping back from close alignment with the conceptual framework and theory of the repository.
- Any readability improvments, including function and struct names, variable names, even code design, that align well with the latest theory and conceptual framework. It should be easy to flow from the docs to the
  yaml specs and schema and into the code and everything should semantically flow in a connected way.
  Make sure you don't diverge from the theoretical, conceptual and architectural grounding of this repo, but also bear in mind that strong alignment to theory creates clarity and simplicity. Look both across the work
  in this session but also adjacent work, older work that it perhaps now out of date, and be very mindful that executor is now quite large and complicated, but the scope is all of the modules in the lib.

After that, remind me to ask you about testing gaps as the next phase.

Keep your candidate list in mind - if anything doesn't get addressed this audit, we'll pick it up next. Definitely include a dead-code sweep as part of this run. WE'll keep the pytest stats suite to run for a
subsequent step as well.

4. Make sure CI passes
4. UBO example
5. Transactional example including u-turn
6. Comparisons
   - Synth
   - SDV (synthetic data vault)
7. Include from repo
8. Import from repo
9. ILP project
10. Better name