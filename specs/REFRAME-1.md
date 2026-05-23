# Reframe.

> Claude, just a hint that we're taking our terminology from order theory and partially ordered sets here, and then taking terminology from wider graph theory for the execution DAG. Everything else comes from existing theory - Bernoulli probabiltiy, IPF algorithm alread yused etc etc.

> Claude, helpful hint, contrary to JOINT-REFRAME-1.md, in this iteration 'atoms' means the mathematical definition - the set of elements in a lattice or semi lattice which are least elements greater than 0 - i.e. the 'bottom row' of a boolean lattice in a Hasse diagram. For us, these will correspond to the set of _most constrainted, most specified_ joint, variant-expanded, flattened datasets that we need to generate!

## High level approach

The current fakeset algorithm is correct and well-staged, but its conceptual framing is fragmented across several abstractions that were introduced incrementally: includes, content includes, pool siblings, inner flats, prefills, collects. Each abstraction made local sense at the time but the vocabulary doesn't immediately reveal the unified structure underneath.

The idea is to build an execution DAG by modelling dataset definitions as concepts within a concept semi-lattice with concept inheritance (standard `include`) as the partial order and joint probability sampling as meet or infinum (greatest lower bound - we use '&' as the symbol for brevity).

We model datasets as semi-lattices since they will have multiple 'tops' -- organisations and indivdiuals for example are different concepts that are not related to each other via ordering (inheritance) realtion. In reality, this will start as a union of C disconnected concept semi-lattices, since it's possible two inheritance chains are not connected by links.

At the end of this, we should have a core complete applicability of key features (e.g. joint modelling) with simple and clearer code which is easier to understand from documentation, and aligned with YAML file concepts.

### During planning

> Claude, note that weight / row count estimation occurs in a similar way, except solidified in this new concept. Please add some clarity around that to this spec.

Initially, the semi lattices only reflect standard inclusion (not link inclusion), and don't model joint distributions between siblings or variants.

- First, the elements are expanded to model concepts from the yaml specification of a dataset: nesting, link includes and variants. We do this by creating extra nodes or elements in the semi-lattice structure, not necessarily concrete datasets.

- Then, joint distributions are modelled by
  - Taking the set of immediate predecessors (lower covers) of an element - i.e. everything which includes it
  - Creating 2^n combinations of the set of immediate predecessors, and using bernoulli weights to re-weight the ratios

> Claude, this is as far as I got here, there are obviously a few more steps. 

#### Planning | Building an execution graph from the semi-lattice structure

As the final stage of planning, the semi-lattice structure is converted into an actual execution graph. This doesn't preserve the ordering relation perfectly but extends it into a structure represented by a DAG.

The primary reason why the union of semi-lattices becomes a DAG is the introduction of *new edges* (which could be thought of as pairwise extension of the ordering relation) which nevertheless changes the infinum to no longer be joint distribution.

> Question for claude: after we've added links to ensure correct execution order in the DAG between linked dataset relations, what *is* the infinum (&) and is it useful to us?

#### During execution

- When executing, we basically have two stages:
  - Generating atoms in the DAG of definitions, in topo order so that dataset include 'links' and 'linked content' is able to expand upon what we previously called pooled data correctly
      - There should be a high degree of parallelism here, since each atom models a joint segment, each atom is actually independent excepting link edges
  - Combining prefills for any element in the DAG (formerly semi-lattice) which is responsble for outputting a dataset.


### High level flow

> Claude: could you please draw some short mermaid diagrams showing these graphs at each stage and how we extend and manipulate them.

*Planning*

1. *Initial set*. Read all the yamls as elements of the set, including files which do not produce output.
   1. An ordering relation is induced on the `include` hierarchy.
   2. For now, this is a poset of N distinct semi-lattices for each include hierarchy- there's nothing stopping users definiting a nice lattice build on 'individuals' and 'organisations' and then a completely disjoint one about 'squid' and 'octopi'. In examples, we'll generally just handwave at this and make sure the implementation handles this top level.

2. *Variants*. Elements with variant fields in their definitions are expended.
   1. Variants are expanded into 2^n variant combinations in the lattice, replacing the original element. Each variant element has the same include from the original (i.e. are order-independent in the semi-lattice and are a subset of a lower cover of the element).
   2. For now, includes ratios for the element are adjusted proportionately to the 'ratio' field in the original variant element field definition.

3. Extra nodes for linked data and nested link content 
   1. As before, linked content lists (formerly nested includes) get split into a new element, which is less in the inclusion hierarchy than the current element (but interpolates any transitively less elements), which is responsible for generaing the 'outer' column values, and an element in the existing position in the semi-lattice which is responsible for 'assembly' into grouped or collected lists and fields including the linked dataset. The pre-seeding element includes the assembly element, the assembly element includes the original include.
   2. Foreign elements for *'Link' includes*. If something in the 'individuals' include chain (e.g. 'directors'), 'links' to an organisation with a requisite cardinality, a higher multiplicity dataset element is generated which includes 'organisations' in the lattice, who is responsible for generating the flat rows with organisation links from the seed (Claude: do you call this pool?) of individual director column values. For now, there is no ordering relation as it would break the semi-lattice structure, but later we will artificially add edges in the DAG to ensure that the organisation include-child can 'use' the data from the directors seed. This foreign element 'includes' whatever dataset the original 'link' pointed at.
      1. Note: we continue to make liberal use of index based tracking in these dataset specs to enable subsequent grouping.
> Note: we create the two kinds of additional 'virtual' element in the lattice _before_ bernoulli factoring now.

4. Joint distribution nodes
    1. As before, every node in the graph except greatest nodes with ratio 1 (implicityl or explicitly) are subject to expansion using product-bernoulli joint probability modelling, removal of contradictory constraint nodes, and IPF reallocation of ratio/size weights.
   2. This proceeds as before - we take the lower cover, add an element if the sum of the ratios < 1 (or row count calculation), and then produce 2^N combinations as a new cover set with the same include.
   3. Ratios for each note use product-bernoulli calculation, then we check the constraints for satisfiability (this is a key concept we're retaining), if they are not, the node is pruned and weights redistributed via IPF, just as currently happens
   4. The same processe happens transitiveyl and recursively for the cover-of-covers etc, so that joint distributions are pushed all the way down.

> Note, the early stopping and pruning optimisations already implemented remain critical for scale, to avoid exponential runaway with the number of segments.

5. Mark atoms
    1. An atom is any element in this lattice which generates data without prefill (not including joins to other previously generated elements by wall time, created by the linked dataset DAG edges below.)
    2. In order theory, an atom is a least element > 0, so it kinda makes sense. In reality, it is likely to be
       - Any 'remainder' generation node (i.e. the 'remainder' content of 'organisations' not covered by other include 'ratios')
       - Likely a highly combined (a&b&c&d&...) segment created via variant factoring and the joint distribution expansion algorithm.

*Planning - DAG creation*

This structure alone is not enough to create an exceution graph, we need another stage:

6. Add extra links to create a DAG that works with linked data.
   - As in step 3, if directors contains a `link` to organisations, then a foreign 'node' for directors is created in the 'organisations' include hierarchy (likely many after factoring and joint distribution modelling!). 
   - To get this right, it is vital that the edges are added to the DAG which ensure the following execution order:
     - The seed (did you call this pool?) column values from the 'individuals' include hierarchy are generated first with the appropriate cardinality and ratios and row size
     - These are then used (new edge) to generated the virtual foreign node in the organisations lower cover set, with the appropriate cardinality (e.g. many for one)
     - Any reducers or collects are then applied grouping up lists in the indivdiuals hierarchy (another new edge)

> Claude, this is as far as I got here, would be interesting see you take a stab at completing. Keep it brief since I just need skeleton to work with.

*Execution*

> Claude we basically need to fill in how have a two phase appraoch; first  we generate atomic nodes in a topo ordering which is nevertheless got some layering / sequencing due to the additional nodes in 6
> ... then we are essentially doing prefill up the DAG, with some sugar (e.g. shuffle) type things on top.

# Glossary

> Claude, please write a glossary of the key terms of our conceptual framing. For example:

For example:
Inheritance: A inherits from B if A's dataset definition has an `include` stanza referencing B. For example, the dataset of cats might inherit from the dataset of animals. Generally, taken as an ordering relation, cats are executed first.

## Concept map

> Claude, please write a table mapping YAML field features to the new conceptual framing, mapped to perhaps some additional implementation details

## Core terminological changes

> Claude, as a table please

E.g. it's probably time to rename sibling set -> lower cover (this is more precise?) Make sense?