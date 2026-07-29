// Regression fixture: syntax that Roslyn expresses with its "omitted" (empty)
// syntax nodes. The preparation must remove those empty *rules* (ANTLR forbids
// an empty rule inside a closure) WITHOUT losing the syntax they expressed —
// deleting the alternatives outright makes every line below fail to parse.
class C
{
    // omitted_array_size_expression: the blank sizes in a multi-dimensional rank
    int[,] _field;

    int[,] MultiDim()
    {
        int[,] local;
        return new int[2, 3];
    }

    int[,,] ThreeDim() { return new int[1, 2, 3]; }

    // omitted_type_argument: unbound generic names
    System.Type Unbound() { return typeof(System.Collections.Generic.Dictionary<,>); }
    System.Type UnboundOne() { return typeof(System.Collections.Generic.List<>); }
}
